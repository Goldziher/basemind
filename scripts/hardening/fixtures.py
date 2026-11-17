"""A small generated fixture repository: code in two languages, two commits, and a document set
(markdown, HTML, a text-layer PDF, an .eml and an .ipynb) each carrying one planted fact.

The facts use invented proper nouns so a hit can only come from the planted document.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path

FILLER = (
    "Routine maintenance notes follow. Inspections are scheduled quarterly and logged in the "
    "facilities register. Staff rotate between sites according to the published roster, and any "
    "deviation is reported to the duty manager before the end of the shift. "
)


@dataclass(frozen=True)
class PlantedFact:
    path: str
    mime: str
    question: str
    answer: str


FACTS = [
    PlantedFact(
        "docs/handbook.md",
        "text/markdown",
        "How often must the Zephyrine reactor coolant be replaced?",
        "37 days",
    ),
    PlantedFact(
        "docs/observatory.html",
        "text/html",
        "When does the Quillfeather observatory open to visitors?",
        "06:45",
    ),
    PlantedFact(
        "docs/bridge-report.pdf",
        "application/pdf",
        "What is the toll for crossing the Marrowind bridge?",
        "eleven copper tokens",
    ),
    PlantedFact(
        "docs/archive-notice.eml",
        "message/rfc822",
        "Who audits the Brightwater archive inventory?",
        "Orla Penhallow",
    ),
    PlantedFact(
        "docs/yield-analysis.ipynb",
        "application/x-ipynb+json",
        "What is the Tamberlyn yield coefficient?",
        "4.82",
    ),
]

RUST_V1 = """//! Fixture crate for the hardening sweep.

/// Something that can be measured.
pub trait Gauge {
    fn reading(&self) -> u32;
}

/// A fixed-value gauge.
pub struct FixedGauge {
    pub value: u32,
}

impl Gauge for FixedGauge {
    fn reading(&self) -> u32 {
        self.value
    }
}

/// Sum the readings of several gauges.
pub fn total_reading(gauges: &[&dyn Gauge]) -> u32 {
    gauges.iter().map(|g| g.reading()).sum()
}

/// Entry point used by the sweep's `code_refs` check.
pub fn compute_total() -> u32 {
    let a = FixedGauge { value: 2 };
    let b = FixedGauge { value: 3 };
    total_reading(&[&a, &b])
}
"""

RUST_V2 = (
    RUST_V1
    + """
/// Added in the second commit so `git_log` and `git_diff` have history to show.
pub fn double_total() -> u32 {
    compute_total() * 2
}
"""
)

PYTHON = '''"""Fixture module for the hardening sweep."""

import json
from pathlib import Path


def load_settings(path):
    """Read a JSON settings file."""
    return json.loads(Path(path).read_text())


def settings_summary(path):
    """Summarise the settings file."""
    settings = load_settings(path)
    return ", ".join(sorted(settings))
'''


def _pdf(lines: list[str]) -> bytes:
    """A one-page PDF with a real text layer (Helvetica, one text object per line)."""

    def esc(s: str) -> str:
        return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")

    ops = ["BT", "/F1 11 Tf", "14 TL", "72 740 Td"]
    for line in lines:
        ops.append(f"({esc(line)}) Tj T*")
    ops.append("ET")
    stream = "\n".join(ops).encode("latin-1")
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R "
        b"/Resources << /Font << /F1 5 0 R >> >> >>",
        b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    ]
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


def _wrap(text: str, width: int = 90) -> list[str]:
    words, lines, cur = text.split(), [], ""
    for w in words:
        if len(cur) + len(w) + 1 > width:
            lines.append(cur)
            cur = w
        else:
            cur = f"{cur} {w}".strip()
    if cur:
        lines.append(cur)
    return lines


def write_documents(root: Path) -> None:
    docs = root / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    (docs / "handbook.md").write_text(
        "# Plant handbook\n\n## Maintenance\n\n"
        + FILLER * 3
        + "\n\n## Coolant\n\nThe Zephyrine reactor coolant must be replaced every 37 days, "
        "without exception, by a certified technician.\n\n## Staffing\n\n" + FILLER * 3 + "\n"
    )
    (docs / "observatory.html").write_text(
        "<!doctype html><html><head><title>Quillfeather observatory</title></head><body>"
        f"<h1>Visitor information</h1><p>{FILLER * 2}</p>"
        "<p>The Quillfeather observatory opens to visitors at 06:45 on Tuesdays and Thursdays.</p>"
        f"<p>{FILLER * 2}</p></body></html>\n"
    )
    pdf_text = (
        FILLER * 2
        + " Bridge tariffs: the toll for crossing the Marrowind bridge is eleven copper tokens per cart. "
        + FILLER
    )
    (docs / "bridge-report.pdf").write_bytes(_pdf(["Marrowind bridge report", "", *_wrap(pdf_text)]))
    (docs / "archive-notice.eml").write_text(
        "From: Records Office <records@example.org>\n"
        "To: Archive Staff <staff@example.org>\n"
        "Subject: Brightwater archive inventory audit\n"
        "Date: Mon, 02 Nov 2026 09:00:00 +0000\n"
        "MIME-Version: 1.0\n"
        "Content-Type: text/plain; charset=utf-8\n\n"
        f"Hello all,\n\n{FILLER}\n\nThe Brightwater archive inventory is audited by Orla Penhallow, "
        "who signs off every shelf register.\n\nRegards,\nRecords Office\n"
    )
    notebook = {
        "cells": [
            {"cell_type": "markdown", "metadata": {}, "source": ["# Yield analysis\n", "\n", FILLER]},
            {
                "cell_type": "markdown",
                "metadata": {},
                "source": ["The Tamberlyn yield coefficient is 4.82, measured across the spring harvest."],
            },
            {
                "cell_type": "code",
                "execution_count": 1,
                "metadata": {},
                "outputs": [],
                "source": ["coefficient = 4.82\n", "print(coefficient * 10)"],
            },
        ],
        "metadata": {"kernelspec": {"name": "python3", "display_name": "Python 3", "language": "python"}},
        "nbformat": 4,
        "nbformat_minor": 5,
    }
    (docs / "yield-analysis.ipynb").write_text(json.dumps(notebook, indent=1))


def _git(root: Path, *args: str) -> None:
    subprocess.run(  # noqa: S603
        ["git", "-c", "user.name=Hardening", "-c", "user.email=hardening@example.invalid", *args],  # noqa: S607
        cwd=root,
        check=True,
        capture_output=True,
    )


def build_fixture(root: Path, with_docs: bool = True) -> Path:
    """Create the fixture repository at `root` with two commits; returns `root`."""
    (root / "src").mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "0.1.0"\nedition = "2021"\n')
    (root / "src" / "lib.rs").write_text(RUST_V1)
    (root / "src" / "settings.py").write_text(PYTHON)
    if with_docs:
        write_documents(root)
    _git(root, "init", "-q", "-b", "main")
    _git(root, "add", "-A")
    _git(root, "commit", "-q", "-m", "feat: initial fixture")
    (root / "src" / "lib.rs").write_text(RUST_V2)
    _git(root, "add", "-A")
    _git(root, "commit", "-q", "-m", "fix: add double_total")
    return root
