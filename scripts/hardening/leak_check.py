#!/usr/bin/env python3
"""Definite-leak check of one-shot basemind paths: macOS `leaks --atExit`, Linux valgrind memcheck.

Runs `basemind scan` (and any extra one-shot command given with --cmd) against the generated
fixture (code + documents), two scale fixtures and, optionally, a copy of a real repository, each
under the platform's leak checker, and reports the leak count, leaked bytes and the allocation
sites. Any leak fails the run; a run the checker could not inspect is an error, not a pass.

The binary must be built with `--features system-allocator`. The shipped allocator, mimalloc,
takes its memory with mmap and registers no malloc zone, so neither checker sees a Rust-heap leak,
and `leaks` (which scans only malloc zones for pointers) reports every C allocation a Rust value
owns, such as the cached tree-sitter queries, as unreferenced. On the system allocator both
checkers see the whole heap, so a report is a real leak and needs no allowlist.

- macOS: `leaks --atExit` with `MallocStackLogging=1`; counts every unreachable block.
- Linux: `valgrind --leak-check=full`; counts "definitely lost" blocks.

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
# A group of several roots ends its tree with a `<< TOTAL >>` line; a single root's tree starts with it.
GROUP_TOTAL = re.compile(r"^\s*(\d+) \(([\d.]+)([KMG]?)(?: bytes)?\) (?:<< TOTAL >>|ROOT )", re.MULTILINE)
UNIT = {"": 1, "K": 1024, "M": 1024**2, "G": 1024**3}

# The scale fixture: Rust and Python files per language for the small and large run. A leak per
# file shows as a count that grows from the small run to the large one.
SCALE_FILES = (60, 600)


def _block_frames(block: str) -> list[str]:
    """basemind's own frames of one stack block, outermost first."""
    own = []
    for line in block.splitlines()[1:]:
        m = FRAME.match(line)
        if m and m.group(1) == "basemind":
            own.append(m.group(2))
    return own


def _site_of(own: list[str]) -> tuple[str, str]:
    """(owner, wrapper): the innermost basemind frame outside tree-sitter's own crate and C code,
    and the outermost tree-sitter C function the stack reached."""
    wrappers = [f for f in own if f.startswith("ts_")]
    callers = [f for f in own if not f.startswith("ts_") and "11tree_sitter" not in f]
    return (callers[-1] if callers else "?"), (wrappers[0] if wrappers else "")


def leak_groups(report: str) -> list[dict]:
    """One record per stack group: root count, leaked nodes and bytes, owner frame and C wrapper."""
    groups = []
    for block in re.split(r"\n(?=STACK OF )", report):
        head = STACK_HEAD.match(block)
        if not head:
            continue
        owner, wrapper = _site_of(_block_frames(block))
        matches = list(GROUP_TOTAL.finditer(block))
        total = next((m for m in matches if "TOTAL" in m.group(0)), matches[0] if matches else None)
        if total:
            nodes, size = int(total.group(1)), int(float(total.group(2)) * UNIT[total.group(3)])
        else:
            nodes, size = int(head.group(1)), 0
        groups.append(
            {
                "roots": int(head.group(1)),
                "nodes": nodes,
                "bytes": size,
                "owner": owner,
                "wrapper": wrapper,
            }
        )
    return groups


def leak_sites(report: str) -> list[tuple[str, int]]:
    """(allocation site, root count) per distinct site: the owner frame, then the allocator call it
    reached."""
    sites: dict[str, int] = {}
    for g in leak_groups(report):
        site = g["owner"] + (f" -> {g['wrapper']}" if g["wrapper"] else "")
        sites[site] = sites.get(site, 0) + g["roots"]
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
        "groups": [],
    }


def judge(result: dict) -> list[str]:
    """Why a `leaks` run fails, empty when it passes: any leaked block, or a leak count the stacks
    do not account for."""
    if not result["inspected"]:
        return ["leaks could not inspect the process"]
    if not result["leaks"]:
        return []
    groups = result["groups"]
    reasons = [
        f"{result['leaks']} leaks ({result['bytes']} bytes) in {len(groups)} stack groups, first at "
        f"`{groups[0]['owner']} -> {groups[0]['wrapper'] or '?'}`"
        if groups
        else f"{result['leaks']} leaks ({result['bytes']} bytes) and no stack (MallocStackLogging off?)"
    ]
    attributed = sum(g["nodes"] for g in groups)
    if groups and attributed < result["leaks"]:
        reasons.append(f"only {attributed} of {result['leaks']} leaks attributed to stacks")
    return reasons


def judge_valgrind(result: dict) -> list[str]:
    """Why a valgrind run fails, empty when it passes: any definitely-lost block."""
    if not result["inspected"]:
        return ["valgrind could not inspect the process"]
    if result["leaks"]:
        return [f"{result['leaks']} blocks ({result['bytes']} bytes) definitely lost"]
    return []


def uses_mimalloc(binary: Path) -> bool:
    """Whether `binary` routes Rust allocations through mimalloc: its global-allocator shim calls
    `mi_malloc_aligned`, which nothing else in basemind does, so the symbol is linked only then."""
    nm = shutil.which("nm")
    if nm is None:
        return False
    out = subprocess.run([nm, str(binary)], capture_output=True, text=True, check=False).stdout  # noqa: S603
    return re.search(r"\b_?mi_malloc_aligned$", out, re.MULTILINE) is not None


def build_scale_fixture(root: Path, files: int) -> Path:
    """`files` Rust and `files` Python modules with distinct content, one commit."""
    src = root / "src"
    src.mkdir(parents=True)
    for i in range(files):
        (src / f"m{i}.rs").write_text(
            f"pub fn f{i}(x: u32) -> u32 {{ g{i}(x) + 1 }}\nfn g{i}(x: u32) -> u32 {{ x * 2 }}\n"
            f"pub struct S{i};\nimpl Clone for S{i} {{ fn clone(&self) -> Self {{ S{i} }} }}\n"
        )
        (src / f"p{i}.py").write_text(f"def p{i}(x):\n    return q{i}(x)\n\n\ndef q{i}(x):\n    return x\n")
    for argv in (["init", "-q"], ["add", "-A"], ["-c", "user.email=h@x", "-c", "user.name=h", "commit", "-qm", "init"]):
        subprocess.run(["git", *argv], cwd=root, check=True)  # noqa: S603,S607
    return root


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
    result["groups"] = leak_groups(proc.stdout)
    return result


def main() -> int:  # noqa: C901, PLR0915
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
    ap.add_argument("--threads", type=int, default=4, help="RAYON_NUM_THREADS for every run (default 4)")
    args = ap.parse_args()
    if args.tool is None or not shutil.which(args.tool):
        sys.stderr.write(
            f"leak_check.py needs macOS `leaks` or Linux `valgrind`; `{args.tool or 'either'}` not found\n"
        )
        return 2
    if uses_mimalloc(args.binary):
        sys.stderr.write(
            f"{args.binary} allocates through mimalloc, whose heap neither checker can see; "
            "build it with `--features system-allocator`\n"
        )
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
    for files in SCALE_FILES:
        iso_s = Isolation(f"leaks-scale{files}")
        isolations.append(iso_s)
        targets.append((f"scale{files}", build_scale_fixture(iso_s.root / "repo", files), iso_s))
    if args.repo:
        iso2 = Isolation("leaks-repo")
        isolations.append(iso2)
        dest = iso2.root / "repo"
        subprocess.run(
            ["git", "clone", "-q", "--local", "--no-hardlinks", str(args.repo.resolve()), str(dest)], check=True
        )  # noqa: S603,S607
        targets.append(("repo", dest, iso2))

    extra = {"RAYON_NUM_THREADS": str(args.threads), **dict(kv.split("=", 1) for kv in args.env)}
    results = []
    for label, target, t_iso in targets:
        for argv in [["scan"], *[shlex.split(c) for c in args.cmd]]:
            name = f"{label}-{'-'.join(argv)}"
            log(f"{args.tool}: basemind {' '.join(argv)} in {label}")
            result = run_one(binary, target, argv, t_iso, out, name, extra)
            result["reasons"] = judge(result) if args.tool == "leaks" else judge_valgrind(result)
            results.append(result)

    ok = all(not r["reasons"] for r in results)
    lines = [
        f"# One-shot leak check: {'PASS' if ok else 'FAIL'}",
        "",
        f"- binary: `{binary}`",
        f"- checker: `{args.tool}`",
        f"- env: {extra}",
        "- pass: no leaked block",
        "",
        "| run | inspected | leaks | bytes | verdict | secs | log |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in results:
        verdict = "pass" if not r["reasons"] else "**FAIL**: " + "; ".join(r["reasons"])
        lines.append(
            f"| {r['label']} | {r['inspected']} | {r['leaks']} | {r['bytes']} | {verdict} | "
            f"{r['secs']} | `{r['log']}` |"
        )
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
