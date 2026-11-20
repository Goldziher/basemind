# basemind manual

basemind indexes a repository once (`basemind scan`) and answers questions about it from that
index: where a symbol is defined, who calls it, what changed, what the documents say. An agent
asks through the MCP tools; a script or a human asks through the CLI. Every MCP tool has a CLI
command that runs the same body and takes the same arguments, so this manual names both.

This text is compiled into the binary: `basemind manual <topic>` works offline, with no index and
no daemon. The skills the plugins ship splice some of these pages in verbatim, so the two cannot
disagree.

## Topics

| Topic | Read it for |
|---|---|
| `basemind manual code` | Code navigation: outlines, symbols, bodies, text and meaning search, references, files. |
| `basemind manual resolution` | What the `resolution` grade on a reference means, and when to trust `total`. |
| `basemind manual git` | History, blame and diffs from the git-history index. |
| `basemind manual documents` | Searching PDFs, Office, HTML, Markdown, config and data files. |
| `basemind manual memory` | Durable notes shared across sessions and agents. |
| `basemind manual agents` | Messaging other agents: threads, inboxes, identity, the daemon. |
| `basemind manual indexing` | Scanning, freshness, notices, the daemon and the cache. |
| `basemind manual paging` | `limit`, `cursor`, token budgets and how a cut result says so. |
| `basemind manual setup` | Onboarding, the MCP server, grammars, hooks, cache and admin commands. |
| `basemind manual data` | Where basemind keeps its data on each platform, and how to uninstall it. |
| `basemind manual exit-codes` | What each CLI exit status means. |
| `basemind manual environment` | The `BASEMIND_*` environment variables an operator sets. |
| `basemind manual troubleshooting` | What to check when a tool is missing, empty or failing. |

## Where to start

1. `basemind status` shows whether this workspace has an index; `basemind scan` builds one.
   `basemind overview` then gives the repository's shape in one call: languages, top-level layout,
   build manifests and their entry points, the largest and most-imported files, HTTP routes,
   naming conventions and recent commits (`--sections` picks some, `--limit` sizes each list).
2. `basemind code outline <path>` before opening a file, `basemind code symbols <name>` to find a
   definition, `basemind code refs <name>` for its call sites, `basemind git log` for history.
3. Add `--json` to any query command for the response the MCP tool returns.
4. `basemind <command> --help` lists a command's flags; this manual explains when to use it.
