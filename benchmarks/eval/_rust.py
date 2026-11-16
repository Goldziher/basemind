"""Rust ground truth: run the `rust_gold` extractor (syn, not tree-sitter) over blobs of a repository.

`rust-analyzer` would resolve references by scope, but it is not pinned by the repository's mise
toolchain and not installed as a rustup component here, and the system `ctags` is BSD ctags, which
has no Rust parser. `rust_gold` parses with `syn`, so the gold shares neither basemind's parser nor
its queries. It is built offline from the cargo cache on first use.

Known misses of this truth, stated once for every Rust generator:

- Name-based, not scope-resolved: a call `x.run()` is a call of every `run`; `Foo::new()` is a call
  of `new`. That matches `code_refs`' documented name matching, not "the callers of this binding".
- Macro bodies are re-parsed as items, statements or comma-separated expressions; a body that parses
  as none (`tokio::select!`, `quote!`, DSL macros) is opaque, and calls and definitions inside it
  are not in the gold (counted as `opaque` records).
- `macro_rules!` bodies, attribute arguments and build-script output are not code here.
- Files that fail to parse with syn (reported as `error` records) contribute nothing.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from collections.abc import Iterable
from pathlib import Path

HERE = Path(__file__).parent
CRATE = HERE / "rust_gold"

# Definition kinds an agent looks up by name (`code_symbols` advertises function, method, struct,
# enum, trait, type, const, module, macro). `impl` blocks, fields and variants are not definitions
# of a name.
NAMED_KINDS = {
    "fn",
    "method",
    "method_decl",
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "assoc_type_decl",
    "const",
    "assoc_const_decl",
    "static",
    "mod",
    "macro",
    "foreign_fn",
    "foreign_static",
    "foreign_type",
}


def binary() -> Path:
    """Path of the release `rust-gold` binary, built offline (from the cargo cache) when missing."""
    override = os.environ.get("BASEMIND_EVAL_RUST_GOLD")
    if override:
        return Path(override)
    target = Path(os.environ.get("CARGO_TARGET_DIR") or Path(tempfile.gettempdir()) / "basemind-eval-target")
    exe = target / "release" / "rust-gold"
    proc = subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "--offline",
            "--locked",
            "--quiet",
            "--manifest-path",
            str(CRATE / "Cargo.toml"),
        ],
        env={**os.environ, "CARGO_TARGET_DIR": str(target)},
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0 or not exe.exists():
        sys.exit(f"building rust_gold failed:\n{proc.stderr}")
    return exe


def records(blobs: Iterable[tuple[str, str]]) -> list[dict]:
    """Every `rust_gold` record for `(name, source)` pairs, in input order."""
    payload = bytearray()
    for name, src in blobs:
        data = src.encode("utf-8")
        payload += f"{name}\n{len(data)}\n".encode() + data
    proc = subprocess.run([str(binary())], input=bytes(payload), capture_output=True, check=False)
    if proc.returncode != 0:
        sys.exit(f"rust_gold failed: {proc.stderr.decode(errors='replace')}")
    return [json.loads(line) for line in proc.stdout.decode("utf-8").splitlines() if line]


def by_type(recs: list[dict], t: str) -> list[dict]:
    return [r for r in recs if r["t"] == t]


def summary(recs: list[dict]) -> str:
    """One stderr line saying what the truth could not see."""
    errors = sorted({r["path"] for r in recs if r["t"] == "error"})
    opaque = sum(1 for r in recs if r["t"] == "opaque")
    return f"rust_gold: {len(errors)} unparsed file(s) {errors[:5]}, {opaque} opaque macro bodies"
