"""Run the contract tests against the generated package in ``.hermes/package``.

``pip-package-hermes/`` holds the hand-written ai-rulez source (``hermes.py``) and these tests; the
installable package (pyproject.toml, plugin.yaml, skills, commands) is generated into
``.hermes/package`` by ``ai-rulez generate``. Put that package first on ``sys.path`` so the tests
exercise exactly what is published, regardless of the working directory or ``PYTHONPATH``.
"""

from __future__ import annotations

import sys
from pathlib import Path

GENERATED_SRC = Path(__file__).resolve().parents[2] / ".hermes" / "package" / "src"

if not (GENERATED_SRC / "basemind_hermes_plugin").is_dir():
    raise RuntimeError(f"{GENERATED_SRC} is missing; run `ai-rulez generate` first")

sys.path.insert(0, str(GENERATED_SRC))
