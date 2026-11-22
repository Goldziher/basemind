# Data and uninstall

basemind writes into a repository only what `basemind init` and `basemind hook install` put there.
Everything else lives in the directories below. Removing them and the install removes basemind.

## The data home

`BASEMIND_DATA_HOME` when set, else the platform data directory:

| Platform | Data home |
|---|---|
| Linux and other Unixes | `$XDG_DATA_HOME/basemind`, else `~/.local/share/basemind` |
| macOS | `~/Library/Application Support/basemind` |
| Windows | `%APPDATA%\basemind\data` |

| Under the data home | Holds | Rebuilt when removed |
|---|---|---|
| `cache/workspaces/` | Each workspace's index views, scan status and `telemetry.jsonl`. | yes |
| `cache/blobs/` | Content-addressed extraction results shared by every workspace. | yes |
| `cache/groups/` | Each repository's group index and document stores. | yes |
| `cache/repos/` | Each repository's memory notes and their vectors. | no |
| `grammars/` | Downloaded tree-sitter grammars, unless `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR` names another directory. | yes |
| `registry/` | The repositories and workspaces the daemon knows. | yes |
| `daemons/` | One record per live daemon. | yes |
| `agents/` | Audit records and worktree locks of `basemind serve --lead`. | no |

`basemind cache stats` shows the size of each part. Keep `cache/repos/` to keep memory notes.

## Outside the data home

| Path | Holds |
|---|---|
| `BASEMIND_COMMS_DIR`, else `comms/` under the platform data directory above | Agent threads, messages and the agent roster, the daemon's socket and `daemon.log`. `BASEMIND_DATA_HOME` does not move it. |
| `~/.config/basemind/agents.toml` (Linux and other Unixes), `~/Library/Application Support/basemind/agents.toml` (macOS), `%APPDATA%\basemind\config\agents.toml` (Windows) | The operator's agent-launch policy. On Unix the home directory comes from the user database, not `HOME` or `XDG_CONFIG_HOME`. |
| `$XDG_CACHE_HOME/basemind/`, else `~/.cache/basemind/` | Downloaded binaries: `versions/<version>/` (plugin launcher), `bin/<version>/` (plugin launchers up to 0.33), `<version>/` (pip wrapper). |
| `$XDG_RUNTIME_DIR/basemind/`, else `${TMPDIR:-/tmp}/basemind-<uid>/` | Per-session markers of the plugin hooks and the status line. |
| `~/.claude/settings.json` | The `statusLine` entry `/bm-statusline` writes. |
| The Hugging Face hub cache, by default `~/.cache/huggingface/hub` (`HF_HUB_CACHE` or `HF_HOME` move it) | Embedding models the documents, search and memory features download. Other programs share it. |
| `XBERG_CACHE_DIR`, else `~/Library/Caches/xberg` (macOS), `$XDG_CACHE_HOME/xberg` or `~/.cache/xberg` (Linux), `%LOCALAPPDATA%\xberg` (Windows) | OCR data of the document extractor. Other programs using it share it. |

## In each repository

- `basemind.toml`, `.config/basemind.toml` or `.config/basemind/config.toml`.
- The rules block from `<!-- BEGIN basemind` to `<!-- END basemind -->` in `CLAUDE.local.md`,
  `AGENTS.local.md`, `CLAUDE.md` or `AGENTS.md`; or the file `.ai-rulez/local/rules/basemind-usage.md`
  or `.ai-rulez/rules/basemind-usage.md`.
- The `mcp__basemind__` and `mcp__plugin_basemind_basemind__` entries of `permissions.allow` in
  `.claude/settings.local.json` or `.claude/settings.json`.
- The `.gitignore` lines covering those `.local` files.
- The git `pre-commit` hook `basemind hook install` wrote. If `pre-commit.bak` exists it holds the
  hook that was replaced (a repeated `--force` keeps each earlier one as `pre-commit.bak.1`,
  `pre-commit.bak.2`, and so on, the highest number being the newest): move the one you want back
  over `pre-commit`. Otherwise delete only `pre-commit`.

## Removing basemind

1. Stop the daemon with `basemind comms stop`. `basemind comms doctor` lists daemons of other
   versions still running; they exit once idle.

2. Remove the per-repository files above from each repository you initialised.

3. Remove the install:

   | Channel | Command |
   |---|---|
   | Homebrew | `brew uninstall basemind` |
   | npm | `npm uninstall -g basemind` or `pnpm remove -g basemind`; the binary sits in the package directory and goes with it. |
   | pip | `pip uninstall basemind`, then the binary cache. |
   | cargo | `cargo uninstall basemind` |
   | GitHub release | Delete the binary and the `lib/` directory beside it. |
   | Claude Code plugin | `/plugin uninstall basemind@basemind`, then `/plugin marketplace remove basemind`, then the binary cache. |
   | Other plugin hosts | The host's plugin removal, then the binary cache. |

4. Remove the data. On Linux:

   ```sh
   rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/basemind" "$HOME/.config/basemind"
   rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/basemind"
   ```

   On macOS (the data home and `agents.toml` share one directory):

   ```sh
   rm -rf "$HOME/Library/Application Support/basemind"
   rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/basemind"
   ```

   Add `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` when you set them. Remove the `statusLine`
   entry from `~/.claude/settings.json`. Remove the model caches only when nothing else uses them.
