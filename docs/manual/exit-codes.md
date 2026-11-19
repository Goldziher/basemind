# Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Runtime error. `basemind doctor` also exits 1 when a check fails. |
| `2` | Usage error: an unknown flag or argument, an invalid `rescan` path, or a destructive command without `--yes`. |
| `3` | Busy: another process holds the workspace writer lock, or the daemon a scan was forwarded to is stopping. Retry later. |
| `4` | A required daemon is unreachable. |
| `130` | Interrupted by Ctrl-C. Work already committed is kept and the index stays consistent. |

A failed command prints `Error:` and the cause chain on stderr. An MCP tool never exits: a failure
the caller can fix is a tool result with `isError` whose text names the fix.
