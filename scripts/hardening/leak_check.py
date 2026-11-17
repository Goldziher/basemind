#!/usr/bin/env python3
"""Definite-leak check of one-shot basemind paths: macOS `leaks --atExit`, Linux valgrind memcheck.

Runs `basemind scan` (and any extra one-shot command given with --cmd) against the generated
fixture (code + documents) and, optionally, a copy of a real repository, each under the
platform's leak checker, and reports the leak count, leaked bytes and the allocation sites. A run
the checker could not inspect is an error, not a pass.

- macOS: `leaks --atExit` with `MallocStackLogging=1`; counts every unreachable block.
- Linux: `valgrind --leak-check=full`; counts "definitely lost" blocks. basemind's global
  allocator is mimalloc, which takes its memory with mmap, so memcheck sees only what goes through
  the C allocator (tree-sitter, the C and C++ dependencies); mimalloc's pages are still scanned
  for pointers, so a C block a Rust value still holds is not reported.

Usage:
    leak_check.py --binary target/release/basemind [--repo PATH] [--cmd "status"] [--tool leaks|valgrind]
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import IS_LINUX, IS_MAC, Isolation, default_out_root, install_cleanup, log, terminate_owned  # noqa: E402
from fixtures import build_fixture  # noqa: E402

SUMMARY = re.compile(r"Process (\d+): (\d+) leaks? for (\d+) total leaked bytes")
STACK_HEAD = re.compile(r"STACK OF (\d+) INSTANCES? OF '([^']+)'")
FRAME = re.compile(r"^\s*\d+\s+(\S+)\s+0x[0-9a-f]+ (.+?)(?: \+ \d+)?\s*$")
VG_DEFINITE = re.compile(r"definitely lost: ([\d,]+) bytes in ([\d,]+) blocks")
VG_NO_LEAKS = "All heap blocks were freed -- no leaks are possible"
VG_RECORD = re.compile(r"([\d,]+) bytes in ([\d,]+) blocks are definitely lost in loss record")
VG_FRAME = re.compile(r"^==\d+==\s+(?:at|by) 0x[0-9A-Fa-f]+: (.+?) \((?:in )?(.+)\)\s*$")
VG_ALLOCATORS = ("malloc", "calloc", "realloc", "operator new", "posix_memalign", "aligned_alloc", "memalign")


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


def _num(s: str) -> int:
    return int(s.replace(",", ""))


def valgrind_sites(report: str) -> list[tuple[str, int]]:
    """(allocation site, block count) per distinct site of the definitely-lost records: the first
    frame past the allocator, then the allocator call it reached."""
    sites: dict[str, int] = {}
    lines = report.splitlines()
    for i, line in enumerate(lines):
        rec = VG_RECORD.search(line)
        if not rec:
            continue
        frames = []
        for follow in lines[i + 1 :]:
            m = VG_FRAME.match(follow)
            if not m:
                if frames:
                    break
                continue
            frames.append(m.group(1))
        callers = [f for f in frames if not f.startswith(VG_ALLOCATORS) and not f.startswith("ts_")]
        site = (callers[0] if callers else "?") + (f" -> {frames[0]}" if frames else "")
        sites[site] = sites.get(site, 0) + _num(rec.group(2))
    return sorted(sites.items(), key=lambda kv: -kv[1])


def run_valgrind(  # noqa: PLR0913, PLR0917
    binary: Path, target: Path, argv: list[str], iso: Isolation, out: Path, label: str, extra_env: dict[str, str]
) -> dict:
    env = iso.env(extra_env)
    log_path = out / f"valgrind-{label}.txt"
    started = time.monotonic()
    proc = subprocess.run(  # noqa: S603
        [  # noqa: S607
            "valgrind",
            "--leak-check=full",
            "--show-leak-kinds=definite",
            "--errors-for-leak-kinds=definite",
            "--num-callers=40",
            f"--log-file={log_path}",
            "--",
            str(binary),
            *argv,
        ],
        cwd=target,
        env=env,
        capture_output=True,
        text=True,
        timeout=3600,
        check=False,
    )
    secs = time.monotonic() - started
    report = log_path.read_text() if log_path.exists() else ""
    (out / f"valgrind-{label}.stdout.txt").write_text(proc.stdout + "\n--- stderr ---\n" + proc.stderr)
    m = VG_DEFINITE.search(report)
    clean = VG_NO_LEAKS in report
    return {
        "label": label,
        "argv": argv,
        "target": str(target),
        "checker_exit": proc.returncode,
        "secs": round(secs, 1),
        "log": str(log_path),
        "inspected": m is not None or clean,
        "leaks": _num(m.group(2)) if m else 0 if clean else None,
        "bytes": _num(m.group(1)) if m else 0 if clean else None,
        "command_failed": proc.returncode != 0,
        "sites": valgrind_sites(report),
    }


def run_leaks(  # noqa: PLR0913, PLR0917
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
        "checker_exit": proc.returncode,
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
    ap.add_argument(
        "--tool",
        choices=("leaks", "valgrind"),
        default="leaks" if IS_MAC else "valgrind" if IS_LINUX else None,
        help="leak checker (default: leaks on macOS, valgrind on Linux)",
    )
    args = ap.parse_args()
    if args.tool is None or not shutil.which(args.tool):
        sys.stderr.write(f"leak_check.py needs macOS `leaks` or Linux `valgrind`; `{args.tool or 'either'}` not found\n")
        return 2
    run_one = run_leaks if args.tool == "leaks" else run_valgrind
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
            log(f"{args.tool}: basemind {' '.join(argv)} in {label}")
            extra = dict(kv.split("=", 1) for kv in args.env)
            results.append(run_one(binary, target, argv, t_iso, out, name, extra))

    ok = all(r["inspected"] and r["leaks"] == 0 for r in results)
    lines = [
        f"# One-shot leak check: {'PASS' if ok else 'FAIL'}",
        "",
        f"- binary: `{binary}`",
        f"- checker: `{args.tool}`",
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
    (out / "leaks.json").write_text(json.dumps({"pass": ok, "tool": args.tool, "runs": results}, indent=2))
    sys.stdout.write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
