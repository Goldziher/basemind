#!/usr/bin/env python3
"""Definite-leak check of one-shot basemind paths under macOS `leaks --atExit`.

Runs `basemind scan` (and any extra one-shot command given with --cmd) against the generated
fixture (code + documents) and, optionally, a copy of a real repository, each under
`leaks --atExit` with `MallocStackLogging=1`, and reports the leak count, leaked bytes and the
first leak stacks. A run where `leaks` could not inspect the process is an error, not a pass.

Linux equivalent (not run by this script): `valgrind --leak-check=full --errors-for-leak-kinds=definite`
or `heaptrack basemind scan`; see docs/hardening.md.

Usage:
    leak_check.py --binary target/release/basemind [--repo PATH] [--cmd "status"]
"""

from __future__ import annotations

import argparse
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import IS_MAC, Isolation, default_out_root, install_cleanup, log, terminate_owned  # noqa: E402
from fixtures import build_fixture  # noqa: E402

SUMMARY = re.compile(r"Process (\d+): (\d+) leaks? for (\d+) total leaked bytes")
STACK_HEAD = re.compile(r"STACK OF (\d+) INSTANCES? OF '([^']+)'")
FRAME = re.compile(r"^\s*\d+\s+(\S+)\s+0x[0-9a-f]+ (.+?)(?: \+ \d+)?\s*$")


def leak_sites(report: str) -> list[tuple[str, int]]:
    """(allocation site, root count) per distinct site: the innermost basemind frame that is not
    a C allocator wrapper, then the allocator call it reached."""
    sites: dict[str, int] = {}
    for block in re.split(r"\n(?=STACK OF )", report):
        head = STACK_HEAD.match(block)
        if not head:
            continue
        own = []
        for line in block.splitlines()[1:]:
            m = FRAME.match(line)
            if m and m.group(1) == "basemind":
                own.append(m.group(2))
        wrappers = [f for f in own if f.startswith("ts_")]
        callers = [f for f in own if not f.startswith("ts_")]
        site = (callers[-1] if callers else "?") + (f" -> {wrappers[0]}" if wrappers else "")
        sites[site] = sites.get(site, 0) + int(head.group(1))
    return sorted(sites.items(), key=lambda kv: -kv[1])


def run_one(  # noqa: PLR0913, PLR0917
    binary: Path, target: Path, argv: list[str], iso: Isolation, out: Path, label: str, extra_env: dict[str, str]
) -> dict:
    env = iso.env({"MallocStackLogging": "1", "MallocStackLoggingNoCompact": "1", **extra_env})
    log_path = out / f"leaks-{label}.txt"
    started = time.monotonic()
    proc = subprocess.run(  # noqa: S603
        ["leaks", "--atExit", "--", str(binary), *argv],  # noqa: S607
        cwd=target,
        env=env,
        capture_output=True,
        text=True,
        timeout=3600,
        check=False,
    )
    secs = time.monotonic() - started
    text = proc.stdout + "\n--- stderr ---\n" + proc.stderr
    log_path.write_text(text)
    m = SUMMARY.search(proc.stdout)
    result = {
        "label": label,
        "argv": argv,
        "target": str(target),
        "leaks_exit": proc.returncode,
        "secs": round(secs, 1),
        "log": str(log_path),
        "inspected": m is not None,
        "leaks": int(m.group(2)) if m else None,
        "bytes": int(m.group(3)) if m else None,
        "command_failed": "exited with status" in proc.stdout and "status 0" not in proc.stdout,
    }
    result["sites"] = leak_sites(proc.stdout)
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--repo", type=Path, help="also scan a local clone of this repository")
    ap.add_argument("--cmd", action="append", default=[], help="extra one-shot command run after scan, e.g. 'status'")
    ap.add_argument("--env", action="append", default=[], help="extra KEY=VALUE for the basemind process")
    ap.add_argument("--out", type=Path)
    args = ap.parse_args()
    if not IS_MAC or not shutil.which("leaks"):
        sys.stderr.write("leak_check.py needs macOS `leaks`; on Linux use valgrind or heaptrack (docs/hardening.md)\n")
        return 2
    binary = args.binary.resolve()
    out = args.out or default_out_root() / time.strftime("leaks-%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)

    isolations: list[Isolation] = []

    def cleanup() -> None:
        for iso in isolations:
            terminate_owned(iso.root)
            iso.cleanup()

    install_cleanup(cleanup)

    targets: list[tuple[str, Path, Isolation]] = []
    iso = Isolation("leaks-fixture")
    isolations.append(iso)
    targets.append(("fixture", build_fixture(iso.root / "repo"), iso))
    if args.repo:
        iso2 = Isolation("leaks-repo")
        isolations.append(iso2)
        dest = iso2.root / "repo"
        subprocess.run(
            ["git", "clone", "-q", "--local", "--no-hardlinks", str(args.repo.resolve()), str(dest)], check=True
        )  # noqa: S603,S607
        targets.append(("repo", dest, iso2))

    results = []
    for label, target, t_iso in targets:
        for argv in [["scan"], *[shlex.split(c) for c in args.cmd]]:
            name = f"{label}-{'-'.join(argv)}"
            log(f"leaks --atExit -- basemind {' '.join(argv)} in {label}")
            extra = dict(kv.split("=", 1) for kv in args.env)
            results.append(run_one(binary, target, argv, t_iso, out, name, extra))

    ok = all(r["inspected"] and r["leaks"] == 0 for r in results)
    lines = [
        f"# One-shot leak check: {'PASS' if ok else 'FAIL'}",
        "",
        f"- binary: `{binary}`",
        f"- extra env: {args.env or 'none'}",
        "",
        "| run | inspected | leaks | bytes | secs | log |",
        "|---|---|---|---|---|---|",
    ]
    for r in results:
        lines.append(f"| {r['label']} | {r['inspected']} | {r['leaks']} | {r['bytes']} | {r['secs']} | `{r['log']}` |")
    for r in results:
        if r["sites"]:
            lines += ["", f"## {r['label']}: leak roots by allocation site", "", "| roots | site |", "|---|---|"]
            lines += [f"| {n} | `{site}` |" for site, n in r["sites"]]
    report = "\n".join(lines) + "\n"
    (out / "leaks.report.md").write_text(report)
    sys.stdout.write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
