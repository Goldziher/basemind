---
priority: high
---

# basemind usage

## basemind — prefer it over grep / read / git

basemind is this repo's indexed context layer. Prefer it BEFORE grep, before reading files to find structure, and before naked `git` — it's the default, not a preference. basemind returns paths, lines, and signatures at a fraction of the tokens of reading source. The index lives in a machine-global cache (`~/.local/share/basemind/`, override `BASEMIND_DATA_HOME`), keyed by workspace and served by a background daemon — nothing is written into the repo, and any number of sessions read and write concurrently.

### Routing

Every capability lives behind one of nine domain tools, dispatched by a required `mode`: `code`,
`graph`, `git`, `memory`, `admin`, `web`, `agents`, `workspace`, `shell`. The CLI mirrors the same
nine groups (`basemind <domain> <mode>`).

| Reach for | Instead of |
|---|---|
| `code` modes `symbols` / `references` / `callers` / `grep` | `grep` / `rg` / opening files to find a symbol |
| `code` mode `outline` / `graph` mode `map` | reading whole files to learn their shape |
| `code` mode `find` (fuzzy path search) | `find` / `fd` / `ls -R` to locate a file by name |
| `git` modes `recent` / `blame_symbol` / `touching` / `diff` | `git log` / `git blame` / `git diff` |
| `agents` modes `post` / `inbox` / `thread_list` | assuming you're the only agent in the repo |
| `workspace` modes `workspaces` / `worktrees` / `claim` | editing a worktree another session may already own |
| `memory` mode `documents` / `web` modes `scrape` / `crawl` / `map` | manually reading PDFs / docs or ad-hoc fetching |
| `code` mode `semantic` | keyword-only guessing at where a concept lives |

### Red flags — stop and re-route

- About to `grep` / `rg`? → `code grep`.
- About to open a file just to find a symbol? → `code outline` / `code symbols`.
- About to `git log` / `git blame`? → `git recent` / `git blame_symbol`.
- Already mapped a file with basemind? Don't re-read it.

### Setup & maintenance

- Install the basemind Claude Code plugin from its marketplace (`/plugin marketplace add Goldziher/basemind`, then install `basemind`).
- Keep basemind current: enable plugin auto-update, or update the binary regularly so the index format and tools stay in sync.
- Re-run `basemind init` (or `/bm-init`) after enabling new capabilities to refresh this block.
