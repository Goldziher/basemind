"""Gold for `find`: a mutated file name -> the file it was derived from.

`code find` is fuzzy file lookup (fzf/fd-style). Each task samples a real tracked file whose stem is
unique in the repo and queries it in one of three mutated forms:

  basename  the exact file name (`widget.py`)
  typo      the stem with one adjacent-character swap or one deleted character (`widgte`)
  abbrev    the consonant skeleton of the stem, keeping its first letter (`wdgt`)

Gold is the single source path; scoring is `ranked` (hit@1/@5, MRR, nDCG).
"""

import argparse

import _common as c

VOWELS = set("aeiouAEIOU")


def mutate(stem: str, kind: str, rng) -> str | None:
    if kind == "typo":
        if len(stem) < 5:
            return None
        i = rng.randrange(1, len(stem) - 1)
        if rng.random() < 0.5 and stem[i] != stem[i + 1]:
            return stem[:i] + stem[i + 1] + stem[i] + stem[i + 2 :]
        return stem[:i] + stem[i + 1 :]
    if kind == "abbrev":
        skeleton = stem[0] + "".join(
            ch for ch in stem[1:] if ch not in VOWELS and ch not in "_-"
        )
        return skeleton if len(skeleton) >= 3 and skeleton != stem else None
    return None


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    c.add_common_args(p, ext_default="")
    p.add_argument(
        "--k", type=int, default=10, help="rank cut-off for the ranked scoring"
    )
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    files = repo.files()
    stems: dict[str, int] = {}
    for f in files:
        stem = f.rsplit("/", 1)[-1].rsplit(".", 1)[0]
        stems[stem] = stems.get(stem, 0) + 1
    unique = [
        f
        for f in files
        if stems[f.rsplit("/", 1)[-1].rsplit(".", 1)[0]] == 1
        and len(f.rsplit("/", 1)[-1]) >= 6
    ]
    rng = repo.rng("find", a.seed)
    tasks = []
    kinds = ["basename", "typo", "abbrev"]
    for path in c.sample(rng, unique, len(unique)):
        if len(tasks) >= a.n:
            break
        base = path.rsplit("/", 1)[-1]
        stem = base.rsplit(".", 1)[0]
        kind = kinds[len(tasks) % len(kinds)]
        query = base if kind == "basename" else mutate(stem, kind, rng)
        if not query:
            continue
        task = {
            "id": f"find-{len(tasks):04d}",
            "mode": "find",
            "args": {"query": query, "limit": a.k},
            "gold": [path],
            "scoring": "ranked",
            "k": a.k,
        }
        if kind == "basename":
            task["baseline"] = {
                "grep": "git ls-files | grep -i -F " + c.shell_quote(query)
            }
        tasks.append(task)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
