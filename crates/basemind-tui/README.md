# basemind-tui

A [ratatui](https://ratatui.rs) terminal front-end for the `basemind-agent` engine. It drives the
engine only through the `AgentClient` transport trait — a pure `App::apply`/`on_key` reducer plus
thin `ui`/`run` IO shells — so the same UI can later attach to a daemon-hosted engine unchanged.

## Run

```bash
# live: bring your own key + model
ANTHROPIC_API_KEY=… BASEMIND_AGENT_MODEL=anthropic/claude-sonnet-4 \
  cargo run -p basemind-tui -- "your first prompt"

# resume the latest session in this repo, or a specific one
cargo run -p basemind-tui -- --continue
cargo run -p basemind-tui -- --resume <session-id>
```

## Testing

The UI is verified deterministically — no network, no API key — in three layers, all driven by a
scripted model behind the `test-util` seam. A `Scenario` is an ordered list of assistant turns loaded
from JSON:

```json
{
  "user": "the message that starts the run (auto-sent)",
  "turns": [
    { "text": "assistant text", "tools": [ { "id": "c1", "name": "shell:exec",
                                             "args": { "command": "echo hi" } } ] },
    { "text": "a closing line" }
  ]
}
```

One turn = one model round. A turn **with** tools runs them and continues; a turn **without** tools
stops the run. The scripted model replays turns in order across the whole session (all user messages)
and streams nothing once exhausted — so provide one stop-turn per user message.

| Layer | Where | What it covers |
|---|---|---|
| Render e2e | `src/e2e.rs` (`#[cfg(test)]`) | Feeds scripted `AgentEvent`s through the real `App`/`ui` and asserts on an in-memory ratatui `TestBackend` frame. No terminal. |
| PTY e2e | `tests/pty_*.rs` (behind `replay`, unix) | Spawns the **real binary** under a pseudo-terminal via `tests/common/mod.rs::PtySession`, parses the live ANSI screen with `vt100`, injects keystrokes, and asserts on the rendered screen — the only layer that exercises `run.rs`'s crossterm raw-mode path. |
| Engine e2e | `../basemind-agent/tests/e2e_scripted.rs` | Drives the full `Session::run` loop + tools + transport with a scripted scenario. |

Run them:

```bash
# render e2e (unit tests) + everything default
cargo test -p basemind-tui

# render + PTY e2e (the PTY tests are gated behind `replay`, unix only)
cargo test -p basemind-tui --features replay
```

The `replay` feature also enables `--replay <scenario.json>`, which runs the whole UI against a
scripted model — the vector the PTY tests spawn. It stays off by default so the scripted mocks never
ship in the binary.

> **`vt100` is pinned to the `0.15` line on purpose.** `0.16` needs `unicode-width ^0.2.1`, which
> conflicts with `ratatui 0.29`'s `unicode-width =0.2.0` pin and fails to resolve. Do not bump it.

New PTY tests build on `PtySession`: use its poll-until helpers (`wait_for` / `expect_screen` /
`refute_after_settle`) rather than fixed sleeps — the UI repaints on a ~33 ms dirty-tick, so a screen
assertion must poll until it holds.
