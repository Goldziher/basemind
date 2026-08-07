//! The `shell` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `spawn` / `send` / `capture` / `kill` / `list` / `broadcast` —
//! dispatched to `helpers_shells::run_shell`. Thin wrapper: the bodies live in `helpers_shells.rs`.
//! The whole module is gated on `feature = "shells"`, because every mode talks to the embedded
//! rmux daemon: with the feature off there is no daemon and the tool is never registered rather
//! than answering with a `not_enabled` stub.

#![cfg(feature = "shells")]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_shells::ShellParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_shells")]
impl BasemindServer {
    // No `output_schema`: the six modes return six different response shapes, and SEP-2106 allows
    // exactly one per tool. Declaring a union would mean nested structs, which schemars emits as
    // `$ref` into `$defs` — the construct that silently dropped the whole registry in GH #50. The
    // per-mode shapes are documented in the description instead. ~keep
    //
    // The annotations are the union of the six modes': `spawn` / `send` / `broadcast` write, and
    // `kill` terminates a process, so the tool can claim neither `read_only_hint` nor
    // `idempotent_hint`, and it must advertise `destructive_hint` — a client that auto-approves
    // non-destructive tools must not be able to reach `kill` through this name. ~keep
    #[tool(
        description = "Run a long-lived background terminal session — a build, a dev server, a \
        test watcher, a REPL — and read its output later, instead of blocking on a one-shot \
        command. Backed by the embedded rmux daemon; sessions are headless by default, while \
        `[shells].visual` can opt into a terminal attachment. `mode` is required. `spawn` starts \
        one: `command` runs through the login shell (e.g. \
        `bash -lc '<command>'`), with optional `cwd` (repo-relative; workspace root by default), \
        `env` (key/value list), and \
        `title`; it returns a stable `session_id` — the handle every other mode addresses — plus \
        an `attach_command` (`rmux attach -t <name>`) an operator can run to watch it. Completed \
        sessions and their pane output remain retained until an explicit `kill` or the shared \
        daemon's idle TTL expires with no live panes, so a short-lived command can still be \
        inspected after it exits. `send` types `text` into a session's stdin, \
        appending a newline so the line executes unless \
        `enter=false` sends a raw keystroke fragment. `capture` reads back the most recent \
        non-blank rows from retained pane history, including output from a command that already \
        exited; `lines` caps the rows at 500 and defaults to 50, so poll it \
        rather than expecting every byte ever written. `kill` terminates a live session or removes a \
        retained completed session, returning `killed=true` when removal succeeds. An already-unknown \
        `session_id` is rejected. `list` needs no \
        arguments and returns every session the daemon currently hosts — the way to recover a \
        `session_id` in a process that did not spawn it, since sessions live in the shared daemon, \
        not in this server. `broadcast` types the same `text` into several `session_ids` at once \
        and returns `delivered`; every id must be live, and an unknown one fails the whole \
        broadcast before any pane is written. Parameters that belong to another mode are rejected, \
        not ignored. Needs --features shells.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn shell(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<ShellParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_shells::run_shell(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}
