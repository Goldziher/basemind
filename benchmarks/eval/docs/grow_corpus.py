"""Add plain-text distractor documents to an eval workspace.

Every distractor paragraph repeats a task query, so distractors outrank the gold documents in both lanes
of an unfiltered search. They are `text/plain`, a type no task filters on, so a filtered task still has
its gold among the allowed documents: with enough of them the corpus holds more chunks than a lane's
candidate cut, and only a filter applied before the cut keeps the gold reachable. Deterministic.

    python3 -I benchmarks/eval/docs/grow_corpus.py TASKS.jsonl WORKSPACE COUNT
"""

import json
import sys
from pathlib import Path

PARAGRAPHS_PER_DOCUMENT = 12


def main() -> None:
    tasks_path, workspace, count = sys.argv[1], Path(sys.argv[2]), int(sys.argv[3])
    with Path(tasks_path).open(encoding="utf-8") as handle:
        queries = [json.loads(line)["args"]["query"] for line in handle if line.strip()]
    out = workspace / "distractors"
    out.mkdir(parents=True, exist_ok=True)
    for doc in range(count):
        paragraphs = []
        for slot in range(PARAGRAPHS_PER_DOCUMENT):
            query = queries[(doc * 7 + slot * 13) % len(queries)]
            paragraphs.append(
                f"Working note {doc}.{slot}. {query} The answer to this is recorded elsewhere; this note only "
                f"repeats the question: {query.lower()} It was raised again at review {doc + slot} and left open."
            )
        (out / f"note-{doc:04d}.txt").write_text("\n\n".join(paragraphs) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
