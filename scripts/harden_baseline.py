"""Compare a harden run's scan and git-history build times with the committed baselines.

Usage:
    python3 scripts/harden_baseline.py <results.ndjson> [--baselines FILE] [--machine LABEL]
        [--features SET] [--strict]

`benchmarks/harden-baselines.json` keys its numbers by a machine label and the cargo features the
numbers were measured with. A run is compared only when a baseline exists for its machine label and
features: a different machine class (a CI runner against an Apple M4) or feature set times a different
thing, so it prints its measurements as a candidate baseline entry and passes. A repository is flagged
when a time exceeds its baseline by more than `threshold_pct` and by more than `min_abs_ms`, the
second bound keeping a sub-second scan's timer granularity from reading as a regression. A
regression is printed (and annotated for GitHub Actions); the exit status is non-zero only with
`--strict`, because shared runners vary by more than the threshold run to run.

The machine label is `--machine`, else `BASEMIND_HARDEN_MACHINE`, else derived: `github-ubuntu-latest`
on a GitHub-hosted Linux x64 runner, `apple-m4` on an Apple M4.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
from pathlib import Path

DEFAULT_BASELINES = Path(__file__).resolve().parents[1] / "benchmarks" / "harden-baselines.json"
METRICS = (("scan_ms", "scan"), ("git_build_ms", "git-history build"))


def detect_machine(env: dict[str, str]) -> str | None:
    """The machine label of this host, or None when it is not a known class."""
    if env.get("BASEMIND_HARDEN_MACHINE"):
        return env["BASEMIND_HARDEN_MACHINE"]
    if env.get("GITHUB_ACTIONS") == "true" and env.get("RUNNER_OS") == "Linux" and env.get("RUNNER_ARCH") == "X64":
        return "github-ubuntu-latest"
    if platform.system() == "Darwin":
        brand = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True, check=False
        ).stdout
        if "Apple M4" in brand:
            return "apple-m4"
    return None


def measurements(results: Path) -> dict[str, dict[str, int]]:
    """Per repository, the last recorded scan and git-history build time in ms."""
    found: dict[str, dict[str, int]] = {}
    for line in results.read_text().splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        entry = {"scan_ms": int(record["scan_elapsed_ms"])}
        history = record.get("git_history")
        if history:
            entry["git_build_ms"] = int(history["build_ms"])
        found[record["repo_name"]] = entry
    return found


def regressions(
    measured: dict[str, dict[str, int]], baseline: dict[str, dict[str, int]], threshold_pct: float, min_abs_ms: int
) -> list[str]:
    """One line per time that exceeds its baseline by more than the threshold and the absolute floor."""
    lines = []
    for repo, now in sorted(measured.items()):
        for key, label in METRICS:
            base = baseline.get(repo, {}).get(key)
            if base is None or key not in now:
                continue
            excess = now[key] - base
            if excess > min_abs_ms and excess * 100 > base * threshold_pct:
                lines.append(
                    f"{repo}: {label} {now[key]} ms vs baseline {base} ms (+{excess * 100 / base:.0f}%, limit {threshold_pct:g}%)"
                )
    return lines


def candidate(machine: str, features: str, measured: dict[str, dict[str, int]]) -> str:
    """A baseline entry for `machine` holding this run's numbers, ready to paste into the file."""
    repos = dict(sorted(measured.items()))
    return json.dumps({machine: {"features": features, "repos": repos}}, indent=2)


def main(argv: list[str]) -> int:
    """Compare the results named in `argv`; return the exit status."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("results", type=Path)
    parser.add_argument("--baselines", type=Path, default=DEFAULT_BASELINES)
    parser.add_argument("--machine")
    parser.add_argument("--features", default=os.environ.get("BASEMIND_HARDEN_FEATURES", "full"))
    parser.add_argument(
        "--strict", action="store_true", default=bool(os.environ.get("BASEMIND_HARDEN_BASELINE_STRICT"))
    )
    args = parser.parse_args(argv)

    measured = measurements(args.results)
    if not measured:
        print(f"error: {args.results} holds no result", file=sys.stderr)
        return 2
    baselines = json.loads(args.baselines.read_text())
    machine = args.machine or detect_machine(dict(os.environ))
    entry = baselines["machines"].get(machine) if machine else None
    if entry is None or entry["features"] != args.features:
        label = machine or "unknown-machine"
        print(
            f"baseline: none for machine {label!r} with features {args.features!r}; not compared. "
            "Candidate entry for benchmarks/harden-baselines.json:"
        )
        print(candidate(label, args.features, measured))
        return 0

    found = regressions(measured, entry["repos"], baselines["threshold_pct"], baselines["min_abs_ms"])
    compared = sum(1 for repo in measured if repo in entry["repos"])
    print(f"baseline: {machine}, features {args.features!r}, {compared} of {len(measured)} repositories compared")
    for line in found:
        print(f"REGRESSION {line}")
        print(f"::warning title=harden baseline::{line}")
    return 1 if found and args.strict else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
