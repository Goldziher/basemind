use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use basemind::config::{self, Config, DocumentsCliOverrides};
use basemind::render::{self, Verbosity};
use basemind::store::{LockHolder, Store};
use basemind::watcher::{BatchKind, WatchBatch};

#[cfg(feature = "agent-tui")]
mod agent_cmd;
mod comms_cli;
mod lang_cli;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod stdio_relay;
#[cfg(feature = "desktop-ui")]
mod ui_cmd;

#[derive(Parser, Debug)]
#[command(
    name = "basemind",
    version,
    about = "File-watcher and code-map generator using tree-sitter",
    long_about = None
)]
struct Cli {
    /// Repository root. Defaults to the current directory.
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Suppress all but hard failures and the summary.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Show every per-file result, including unchanged and skipped files.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Force-disable ANSI colors. NO_COLOR env var is honored automatically.
    #[arg(long, global = true)]
    no_color: bool,

    /// Emit machine-readable JSON instead of the human-readable rendering. Applies
    /// to the tool subcommands (query / git / memory / web / telemetry / cache) and
    /// is ignored — with a warning — on init / scan / rescan / watch / hook / lang.
    #[arg(long, global = true)]
    json: bool,

    /// Which view to query or serve. "working" (default) is the on-disk tree;
    /// "staged" is the git index; "rev-<sha7>" is a previously scanned rev. Used by
    /// the tool subcommands and `serve`; ignored — with a warning — elsewhere.
    #[arg(long, global = true, default_value_t = basemind::store::VIEW_WORKING.to_string())]
    view: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialize (or refresh) basemind onboarding: write basemind.toml, gitignore the cache, and
    /// inject a "prefer basemind over grep/read/git" rules block. Re-runnable and idempotent.
    Init(basemind::cli::init::InitArgs),
    /// Run a one-shot scan over the repository and write the code map.
    Scan(ScanArgs),
    /// Re-index the working tree (full) or only the given paths (incremental). Use after
    /// edits, or to rebuild a stale/empty index without starting the server.
    Rescan(RescanArgs),
    /// Long-running watcher; keeps the code map current as files change.
    Watch,
    /// Read the code map: outline, symbols, grep, files, find, definition, references, callers,
    /// implementations, dependents, expand, semantic, chunk.
    #[command(subcommand)]
    Code(basemind::cli::code::CodeCmd),
    /// Git history / blame / diff queries.
    #[command(subcommand)]
    Git(basemind::cli::git::GitCmd),
    /// Navigate the code graph (calls, neighbors, path, subgraph, communities, map, export, …).
    #[command(subcommand)]
    Graph(basemind::cli::graph::GraphCmd),
    /// Shared agent memory + document search (needs `--features memory,documents`).
    #[command(subcommand)]
    Memory(basemind::cli::memory::MemoryCmd),
    /// On-demand web ingestion (needs `--features crawl`).
    #[command(subcommand)]
    Web(basemind::cli::web::WebCmd),
    /// Headless agent shell sessions: spawn / send / capture / kill / list / broadcast
    /// (needs `--features shells`).
    #[cfg(all(feature = "shells", any(unix, windows)))]
    #[command(subcommand)]
    Shell(basemind::cli::shell::ShellCmd),
    /// Server + cache administration: status, repo, rescan, caches, telemetry, compression.
    #[command(subcommand)]
    Admin(basemind::cli::admin::AdminCmd),
    /// Install a pre-commit hook that runs `basemind scan --staged`.
    Hook {
        #[command(subcommand)]
        action: HookCmd,
    },
    /// Manage downloaded tree-sitter grammars.
    Lang {
        #[command(subcommand)]
        action: LangCmd,
    },
    /// Compress verbose command output read from stdin into a compact summary,
    /// failing open (raw passthrough) on errors and preserving credentials.
    CompressOutput(basemind::textcompress::cli::CompressOutputArgs),
    /// Emit a compact `+N/-M` line-diff from a prior file version (`--old`) to
    /// new content read from stdin — the stateless delta re-read primitive.
    Delta(basemind::textcompress::cli::DeltaArgs),
    /// Extract a compact, credential-safe checkpoint (decisions / errors /
    /// changed files) from session text read from stdin; changed files come
    /// from the git working tree, not the text.
    Checkpoint(basemind::textcompress::cli::CheckpointArgs),
    /// Flag wasteful tool usage (redundant reads, repeated queries, oversized
    /// reads) from a JSON-Lines tool-call log read from stdin. Pure analysis.
    DetectWaste(basemind::textcompress::cli::DetectWasteArgs),
    /// Run an MCP server for a stdio client: ensure the daemon (the real server) is up, then relay
    /// this process's stdin/stdout to it. HTTP-native clients skip this and dial the daemon URL
    /// directly (see `daemon ensure`).
    Serve(ServeArgs),
    /// Launch the basemind agent TUI (a coding agent over the code map, needs `--features
    /// agent-tui`).
    #[cfg(feature = "agent-tui")]
    Agent(AgentArgs),
    /// Launch the basemind desktop UI (an interactive code graph over the code map, needs
    /// `--features desktop-ui`).
    #[cfg(feature = "desktop-ui")]
    Ui(UiArgs),
    /// Print a compact one-line summary of the daemon's currently-hot workspaces, for a shell
    /// statusline. Fast and silent: prints nothing and exits 0 when no daemon is running.
    Statusline,
    /// Manage the `.basemind/` caches (gc / stats / clear). Offline path.
    #[command(subcommand)]
    Cache(basemind::cli::admin::CacheCmd),
    /// Manage the user-global agent-comms broker daemon (needs `--features comms`).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Comms {
        #[command(subcommand)]
        action: CommsLifecycleCmd,
    },
    /// Multi-agent coordination: identities, scoped threads, messages, inbox (needs
    /// `--features comms`). Talks to the broker daemon directly, like `comms`.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Agents {
        #[command(subcommand)]
        action: basemind::cli::agents::AgentsCmd,
    },
    /// Machine-registry coordination: workspaces / worktrees / branches / advisory claims (needs
    /// `--features comms`). Talks to the broker daemon directly, like `comms`.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Workspace {
        #[command(subcommand)]
        action: basemind::cli::registry::WorkspaceCmd,
    },
    /// Manage the daemon that hosts the streamable-HTTP MCP transport (needs `--features comms`).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
}

/// Subcommands for `basemind daemon`: the streamable-HTTP MCP transport lifecycle.
#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Ensure the daemon is running and its HTTP transport is ready, then print the base MCP URL.
    /// Idempotent — a no-op when a current daemon already answers.
    Ensure,
}

/// Subcommands for `basemind comms`: the broker daemon's lifecycle only.
///
/// These manage the singleton broker process (`Daemon`/`Start`/`Stop`/`Status`/`Doctor`). The agent
/// coordination verbs it used to carry now live in their own group, `basemind agents` (see
/// `cli::agents`).
#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Subcommand, Debug)]
enum CommsLifecycleCmd {
    /// Run the broker loop: bind the singleton socket, serve front-ends, block until shutdown.
    Daemon,
    /// Ensure the daemon is running (spawn if needed); noop when already alive.
    Start,
    /// Ask the running daemon to drain and stop. With `--all`, stop EVERY live daemon registered on
    /// this machine (reclaims a pile-up), not just the one for the current comms dir.
    Stop {
        /// Stop every live daemon on this machine, not just the current one.
        #[arg(long)]
        all: bool,
    },
    /// Report the daemon's pid / version / uptime / room + subscriber counts.
    Status,
    /// List every live daemon registered on this machine (pid / comms dir / version / uptime) and
    /// flag any pile-up over the ceiling. Prunes dead registry entries as a side effect. Reports
    /// process liveness only unless --probe is given.
    Doctor {
        /// Also ask each comms daemon whether it can actually serve, and report the verdict per
        /// row. Off by default so the plain report stays RPC-free and safe on a wedged machine.
        #[arg(long)]
        probe: bool,
        /// Acknowledge and delete the recorded fatal store error for this comms dir.
        #[arg(long)]
        clear_fatal: bool,
    },
}

#[derive(clap::Args, Debug)]
struct ScanArgs {
    /// Index the git staging area instead of the working tree. Used by the
    /// pre-commit hook so the cache reflects what's about to be committed.
    /// Mutually exclusive with --rev.
    #[arg(long, conflicts_with = "rev")]
    staged: bool,
    /// Index the tree at the given revision (HEAD, branch name, sha, HEAD~3).
    /// Writes under .basemind/views/rev-<sha7>/ — separate from the working-tree view.
    #[arg(long, value_name = "REV")]
    rev: Option<String>,
    /// Skip building the git-history index after the scan (overrides config). The history tools
    /// then fall back to the live walk. Equivalent to `BASEMIND_GH_INDEX=0`.
    #[arg(long)]
    no_git_history: bool,
    /// Wipe and fully rebuild the git-history index instead of incrementally syncing it. Use after
    /// a history rewrite if revalidation didn't already trigger a rebuild.
    #[arg(long)]
    rebuild_git_history: bool,
    /// Document-tier overrides. Every flag in this group corresponds to a
    /// `[documents.…]` TOML key and a `BASEMIND_DOCUMENTS_…` env var.
    #[command(flatten)]
    documents: DocumentsCliOverrides,
}

#[derive(clap::Args, Debug)]
struct RescanArgs {
    /// Repo-relative paths to re-index incrementally. When omitted (or with `--full`),
    /// the entire working tree is re-indexed. Paths are forward-slash with no leading `/`.
    #[arg(value_name = "PATH")]
    paths: Vec<String>,
    /// Force a full working-tree re-index even when paths are supplied. Use to rebuild a
    /// stale or empty index from scratch.
    #[arg(long)]
    full: bool,
    /// Skip building the git-history index after the rescan (overrides config).
    #[arg(long)]
    no_git_history: bool,
    /// Wipe and fully rebuild the git-history index instead of incrementally syncing it.
    #[arg(long)]
    rebuild_git_history: bool,
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    /// LRU capacity per category for the in-process git cache (commit_files, log, blame).
    #[arg(long, default_value_t = 1024)]
    git_cache_mem: usize,
    /// Disable the on-disk git cache. RAM LRU still applies but nothing persists between
    /// `basemind serve` runs.
    #[arg(long)]
    no_git_cache_disk: bool,
    /// Disable the continuous background re-scan. By default `serve` watches the
    /// working tree and incrementally refreshes the index as files change, so the
    /// code map stays current without `rescan`. Pass `--no-watch` to turn that off
    /// for very large repos (e.g. the ~81k-file TypeScript tree) or CI runs where
    /// the per-edit incremental scan isn't worth the cost; refresh manually via the
    /// `rescan` tool instead.
    #[arg(long)]
    no_watch: bool,
    /// Document-tier overrides. Every flag in this group corresponds to a
    /// `[documents.…]` TOML key and a `BASEMIND_DOCUMENTS_…` env var.
    #[command(flatten)]
    documents: DocumentsCliOverrides,
}

#[cfg(feature = "agent-tui")]
#[derive(clap::Args, Debug)]
struct AgentArgs {
    /// Arguments forwarded verbatim to the sibling `basemind-tui` binary: positional `[prompt]`,
    /// plus `--resume <id>`, `--continue`, `--daemon`, `--attach`, `--replay <scenario.json>`. The
    /// environment (including `BASEMIND_AGENT_MODEL` / `ANTHROPIC_API_KEY`) is inherited unchanged.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[cfg(feature = "desktop-ui")]
#[derive(clap::Args, Debug)]
struct UiArgs {
    /// Arguments forwarded verbatim to the sibling `basemind-ui` binary (e.g. `--root <path>`). The
    /// environment is inherited unchanged.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum LangCmd {
    /// Show installed grammars and where they live.
    List,
    /// Force-download all supported grammars (no-op if already cached).
    Install,
    /// Delete the grammar cache. Next run will redownload.
    Clean,
}

#[derive(Subcommand, Debug)]
enum HookCmd {
    /// Write .git/hooks/pre-commit that invokes `basemind scan`.
    Install,
}

/// Default tracing directive when `RUST_LOG` is unset, derived from the parsed
/// verbosity. `--quiet` raises the threshold to `warn` so subsystem INFO logs are
/// suppressed during a scan; `--verbose` lowers it to `debug`; otherwise `info`.
/// An explicit `RUST_LOG` always wins (callers honor it before this fallback).
fn default_log_directive(verbosity: Verbosity) -> &'static str {
    match verbosity {
        Verbosity::Quiet => "warn",
        Verbosity::Default => "info",
        Verbosity::Verbose => "debug",
    }
}

fn main() -> Result<()> {
    let process_started = std::time::Instant::now();
    #[cfg(all(feature = "shells", any(unix, windows)))]
    if let Some(result) = basemind::shells::intercept_internal_reexec() {
        return result;
    }
    let cli = Cli::parse();
    let verbosity = Verbosity::from_flags(cli.quiet, cli.verbose);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_log_directive(verbosity))),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let no_color = cli.no_color;
    let start = cli
        .root
        .clone()
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let root = basemind::config::discover_root_with_basemind(&start);
    if root != start {
        tracing::info!(
            resolved_root = %root.display(),
            from = "ancestor .basemind",
            "resolved repo root upward"
        );
    }

    let json = cli.json;
    let view = cli.view.clone();
    warn_ignored_global_flags(&cli.cmd, json, &view);
    let dispatch = |tc| {
        basemind::cli::run(
            &root,
            &view,
            DocumentsCliOverrides::default(),
            json,
            process_started,
            tc,
        )
    };
    match cli.cmd {
        Cmd::Init(args) => basemind::cli::init::run(&basemind::config::init_root(&start), &args),
        Cmd::Scan(args) => cmd_scan(&root, &args, verbosity, no_color),
        Cmd::Rescan(args) => cmd_rescan(&root, &args, verbosity, no_color),
        Cmd::Watch => cmd_watch(&root, verbosity, no_color),
        Cmd::Code(c) => {
            let _ = basemind::lang::ensure_grammars();
            dispatch(basemind::cli::ToolCmd::Code(c))
        }
        Cmd::Git(g) => dispatch(basemind::cli::ToolCmd::Git(g)),
        Cmd::Graph(g) => {
            let _ = basemind::lang::ensure_grammars();
            dispatch(basemind::cli::ToolCmd::Graph(g))
        }
        Cmd::Memory(m) => dispatch(basemind::cli::ToolCmd::Memory(m)),
        Cmd::Web(w) => dispatch(basemind::cli::ToolCmd::Web(w)),
        #[cfg(all(feature = "shells", any(unix, windows)))]
        Cmd::Shell(s) => dispatch(basemind::cli::ToolCmd::Shell(s)),
        Cmd::Admin(a) => dispatch(basemind::cli::ToolCmd::Admin(a)),
        Cmd::Hook { action } => match action {
            HookCmd::Install => cmd_hook_install(&root),
        },
        Cmd::Lang { action } => match action {
            LangCmd::List => lang_cli::cmd_lang_list(no_color),
            LangCmd::Install => lang_cli::cmd_lang_install(verbosity, no_color),
            LangCmd::Clean => lang_cli::cmd_lang_clean(),
        },
        Cmd::CompressOutput(args) => basemind::textcompress::cli::run(&args),
        Cmd::Delta(args) => basemind::textcompress::cli::run_delta(&args),
        Cmd::Checkpoint(args) => basemind::textcompress::cli::run_checkpoint(&root, &args),
        Cmd::DetectWaste(args) => basemind::textcompress::cli::run_detect_waste(&args),
        Cmd::Serve(args) => cmd_serve(&root, &view, &args, json),
        #[cfg(feature = "agent-tui")]
        Cmd::Agent(args) => agent_cmd::run(&root, &args.args),
        #[cfg(feature = "desktop-ui")]
        Cmd::Ui(args) => ui_cmd::run(&root, &args.args),
        Cmd::Cache(action) => basemind::cli::run_cache(&root, action, json),
        // An explicit `--root` selects the per-repo line for that (resolved) workspace; bare
        // `basemind statusline` keeps the daemon hot-workspace summary.
        Cmd::Statusline => comms_cli::cmd_statusline(cli.root.as_ref().map(|_| root.as_path())),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Comms { action } => comms_cli::cmd_comms(action, json),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Agents { action } => basemind::cli::agents::run(&root, json, action),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Workspace { action } => basemind::cli::registry::run(&root, json, action),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Daemon { action } => comms_cli::cmd_daemon(action, json),
    }
}

/// Emit a `WARN` when a global flag was supplied to a subcommand that does not
/// consume it. `--json` only affects the tool subcommands (query / git / memory /
/// web / telemetry / cache); `--view` additionally affects `serve`. Everything else
/// ignores them, so warning prevents a no-op flag from looking effective.
fn warn_ignored_global_flags(cmd: &Cmd, json: bool, view: &str) {
    let consumes_json = matches!(
        cmd,
        Cmd::Code(_) | Cmd::Git(_) | Cmd::Graph(_) | Cmd::Memory(_) | Cmd::Web(_) | Cmd::Admin(_) | Cmd::Cache(_)
    );
    #[cfg(all(feature = "comms", any(unix, windows)))]
    let consumes_json = consumes_json
        || matches!(
            cmd,
            Cmd::Comms { .. } | Cmd::Agents { .. } | Cmd::Workspace { .. } | Cmd::Daemon { .. }
        );
    #[cfg(all(feature = "shells", any(unix, windows)))]
    let consumes_json = consumes_json || matches!(cmd, Cmd::Shell(_));
    let consumes_view = consumes_json || matches!(cmd, Cmd::Serve(_));

    if json && !consumes_json {
        tracing::warn!("--json has no effect on this subcommand; ignoring");
    }
    if view != basemind::store::VIEW_WORKING && !consumes_view {
        tracing::warn!(view = %view, "--view has no effect on this subcommand; ignoring");
    }
}

fn bootstrap_grammars(verbosity: Verbosity, no_color: bool) -> Result<()> {
    let summary = basemind::lang::ensure_grammars().map_err(|e| anyhow::anyhow!("grammar bootstrap failed: {e}"))?;
    let mut out = render::stdout(no_color);
    render::render_grammar_bootstrap(&mut out, &summary, verbosity);
    Ok(())
}

fn load_or_default(root: &std::path::Path) -> Result<Config> {
    load_or_default_with(root, None)
}

/// Refuse a workspace root that is not a project (issue #62). Root discovery still falls back to
/// the start directory, so a host launched at `/` (or any stray directory) resolves to a root that
/// basemind would open read-write and walk in full. Every verb that opens a root for indexing —
/// and `serve`, which does it a hop away in the daemon — gates on this first, so the operator gets
/// the guidance instead of an OOM.
fn guard_workspace_root(root: &std::path::Path) -> Result<()> {
    match config::root_guard::workspace_root_verdict(root) {
        Ok(()) => Ok(()),
        Err(refusal) => anyhow::bail!(config::root_guard::refusal_message(root, refusal)),
    }
}

/// Variant of [`load_or_default`] that also applies a CLI override layer through
/// the layered merger. Used by `scan` / `serve` to flow `#[command(flatten)]`
/// flags down to the resolved config.
fn load_or_default_with(root: &std::path::Path, cli: Option<DocumentsCliOverrides>) -> Result<Config> {
    match config::load_with_overrides(root, None, cli) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => {
            tracing::info!("no basemind.toml; using defaults");
            Ok(config::default_for_root(root))
        }
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Open the store for a writer command (`scan` / `rescan`), translating lock contention
/// into actionable guidance. Two distinct holders can deny the lock — our own `fs2`
/// advisory lock and Fjall's internal exclusive open lock — and a raw `FjallError: Locked`
/// or bare "Locked" is opaque to a user whose editor plugin is quietly running `serve`.
/// `is_lock_contention` collapses both into one friendly message that leads with what to
/// do; the underlying `StoreError` is preserved as the error source (visible under `-v` /
/// the full anyhow chain) so we never swallow the cause.
fn open_store_for_write(root: &std::path::Path, view: &str, what: &str, holder: LockHolder) -> Result<Store> {
    Store::open_with_holder(root, view, holder).map_err(|err| {
        if err.is_lock_contention() {
            anyhow::Error::new(err).context(basemind::store::LOCK_CONTENTION_HELP.to_string())
        } else {
            anyhow::Error::new(err).context(format!("open store ({what})"))
        }
    })
}

/// Pre-flight the store write lock before a CLI `scan` / `rescan`. When a live basemind process
/// already holds it — overwhelmingly the "editor plugin runs `serve` while the user (or another
/// plugin command) runs `scan`" double-run — return an actionable message so the caller prints it
/// and exits cleanly, instead of blocking on the acquire retries and then failing with a raw lock
/// error. `None` means the lock is free (proceed); the acquire still handles the probe→acquire race
/// reactively via [`basemind::store::LOCK_CONTENTION_HELP`].
fn writer_collision_notice(root: &std::path::Path) -> Option<String> {
    let basemind_dir = basemind::store::workspace_cache_dir(root);
    match basemind::store::probe_writer_lock(&basemind_dir) {
        basemind::store::WriterProbe::Free => None,
        basemind::store::WriterProbe::Held { holder: Some(meta) } => Some(format!(
            "`{}` (pid {}) is already running against this repo and keeping the index fresh — \
             running this directly is unnecessary and would collide with it. Use that server's \
             `rescan` tool to refresh the index, or stop it first.",
            meta.command, meta.pid
        )),
        basemind::store::WriterProbe::Held { holder: None } => Some(basemind::store::LOCK_CONTENTION_HELP.to_string()),
    }
}

/// Build / refresh the repo-global git-history index after a working-tree scan (a separate phase
/// from the core file scan). Best-effort: a non-git dir, a disabled toggle, or any failure leaves
/// the index untouched and the history tools fall back to the live walk — never fails the scan.
fn sync_git_history_after_scan(
    root: &std::path::Path,
    cli_enabled: bool,
    force_rebuild: bool,
    out: &mut impl std::io::Write,
) {
    if !cli_enabled || !basemind::git_history::index_enabled() {
        return;
    }
    let Ok(repo) = basemind::git::Repo::discover(root) else {
        return;
    };
    let basemind_dir = basemind::git_history::shared_history_basemind_dir(root);
    let index = match basemind::git_history::GitHistoryIndex::open(&basemind_dir) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(?error, "git-history index unavailable; skipping");
            return;
        }
    };
    if force_rebuild && let Err(error) = index.clear(&basemind_dir) {
        tracing::warn!(?error, "git-history index clear failed");
    }
    match basemind::git_history::builder::sync(&index, &repo, &basemind_dir) {
        Ok(outcome) => {
            let summary = match outcome {
                basemind::git_history::builder::RebuildOutcome::Fresh => "git-history index: up to date".to_string(),
                basemind::git_history::builder::RebuildOutcome::Incremental { added } => {
                    format!("git-history index: +{added} commits")
                }
                basemind::git_history::builder::RebuildOutcome::FullRebuild { reason, commits } => {
                    format!("git-history index: rebuilt {commits} commits ({reason})")
                }
            };
            let _ = writeln!(out, "{summary}");
        }
        Err(error) => tracing::warn!(?error, "git-history index sync failed"),
    }
    // Fjall 3.1.9 can deadlock in `DatabaseInner::drop` even after writers stop
    // (fjall-rs/fjall#260). A scan is a short-lived helper process, so let the OS close this
    // already-persisted database at process exit instead of risking an indefinitely wedged CLI.
    std::mem::forget(index);
}

fn cmd_scan(root: &std::path::Path, args: &ScanArgs, verbosity: Verbosity, no_color: bool) -> Result<()> {
    guard_workspace_root(root)?;
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default_with(root, Some(args.documents.clone()))?;

    let mut out = render::stdout(no_color);
    if args.staged {
        let repo = basemind::git::Repo::discover(root).context("`--staged` requires being inside a git repository")?;
        let mut store = open_store_for_write(root, basemind::store::VIEW_STAGED, "staged", LockHolder::Scan)?;
        render::render_scan_header(&mut out, "staged index", verbosity);
        let report = basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::Staged(&repo),
            basemind::scanner::EmbedMode::Inline,
        )
        .context("scan staged")?;
        render::render_report(&mut out, &report, verbosity);
        return Ok(());
    }
    if let Some(rev_spec) = &args.rev {
        let repo = basemind::git::Repo::discover(root).context("`--rev` requires being inside a git repository")?;
        let sha = repo.resolve_rev(rev_spec).context("resolve rev")?;
        let short = &sha[..7.min(sha.len())];
        let view = basemind::store::view_name_for_rev(short);
        let mut store = open_store_for_write(root, &view, "rev", LockHolder::Scan)?;
        render::render_scan_header(&mut out, &format!("rev {short}"), verbosity);
        let report = basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::Rev {
                repo: &repo,
                sha: sha.clone(),
            },
            basemind::scanner::EmbedMode::Inline,
        )
        .context("scan rev")?;
        render::render_report(&mut out, &report, verbosity);
        return Ok(());
    }

    if let Some(notice) = writer_collision_notice(root) {
        use std::io::Write as _;
        render::render_scan_header(&mut out, "scan", verbosity);
        let _ = writeln!(out, "{notice}");
        return Ok(());
    }
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "scan", LockHolder::Scan)?;
    let report = basemind::scanner::scan(
        root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .context("scan")?;
    render::render_report(&mut out, &report, verbosity);
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}

fn cmd_rescan(root: &std::path::Path, args: &RescanArgs, verbosity: Verbosity, no_color: bool) -> Result<()> {
    guard_workspace_root(root)?;
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default(root)?;
    let mut out = render::stdout(no_color);
    if let Some(notice) = writer_collision_notice(root) {
        use std::io::Write as _;
        let _ = writeln!(out, "{notice}");
        return Ok(());
    }
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "rescan", LockHolder::Rescan)?;

    let report = if args.full || args.paths.is_empty() {
        basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
        )
        .context("rescan (full)")?
    } else {
        let abs: Vec<PathBuf> = args.paths.iter().map(|p| root.join(p)).collect();
        basemind::scanner::scan_paths(root, &mut store, &config, &abs, basemind::scanner::EmbedMode::Inline)
            .context("rescan (paths)")?
    };
    render::render_report(&mut out, &report, verbosity);
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}

fn cmd_watch(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    guard_workspace_root(root)?;
    bootstrap_grammars(verbosity, no_color)?;
    let config = Arc::new(load_or_default(root)?);
    let store = Arc::new(Mutex::new(
        Store::open_with_holder(root, basemind::store::VIEW_WORKING, LockHolder::Watch).context("open store")?,
    ));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let store_w = Arc::clone(&store);
    let config_w = Arc::clone(&config);
    let root_buf = root.to_path_buf();
    let watcher_handle = std::thread::spawn(move || {
        let mut stdout = render::stdout(no_color);
        let cb: basemind::watcher::BatchCallback = Box::new(move |batch: WatchBatch<'_>| match batch.kind {
            BatchKind::InitialScan => {
                render::render_report(&mut stdout, batch.report, verbosity);
            }
            BatchKind::Incremental { paths } => {
                render::render_batch_header(&mut stdout, paths, verbosity);
                render::render_lines(&mut stdout, batch.report, verbosity);
            }
        });
        basemind::watcher::watch(&root_buf, store_w, config_w, shutdown_rx, cb)
    });

    runtime.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl-c received; shutting down");
        let _ = shutdown_tx.send(());
    });
    match watcher_handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Err(anyhow::anyhow!("watcher thread panicked")),
    }
}

/// Dispatch `basemind serve`: a thin stdio↔daemon relay, not a server in its own right.
///
/// The comms daemon is the sole MCP server — it hosts the rmcp router over BOTH a Unix-socket relay
/// and a streamable-HTTP front-end. This verb ensures that daemon is up, then byte-pumps this
/// process's stdin/stdout to the daemon's relay socket, so any MCP client that speaks stdio reaches
/// the daemon-hosted router. The daemon outlives any single client: when the client disconnects the
/// pump ends and `serve` exits, but the daemon (and its warm index) stays up for the next client —
/// which is why a dropped stdio connection no longer bricks the workspace (the old in-process stdio
/// server, whose lifetime was bound to the pipe, was the "drops and never returns" bug). HTTP-native
/// clients skip `serve` entirely and dial the daemon URL directly (see `basemind daemon ensure`).
/// Without the `comms` feature there is no daemon to relay to, so it errors with guidance.
fn cmd_serve(root: &std::path::Path, view: &str, args: &ServeArgs, json: bool) -> Result<()> {
    // `ServeArgs` (git-cache / `--no-watch` / documents) configure the daemon-hosted workspace, not
    // this thin relay; the daemon honors them when it first builds the workspace. `--json` is a
    // rendering flag for the tool subcommands and has no meaning for a raw stdio relay.
    let _ = (args, json);
    // Bug #18 guard (transport-independent): a named view that was never scanned must fail fast with
    // actionable guidance instead of implying a server for an index that does not exist. The working
    // view is exempt (it auto-scans on first daemon touch).
    if view != basemind::store::VIEW_WORKING {
        let index_path = basemind::store::workspace_cache_dir(root)
            .join(basemind::store::VIEWS_DIR)
            .join(view)
            .join(basemind::store::INDEX_FILE);
        if !index_path.exists() {
            anyhow::bail!(
                "view {view:?} has not been scanned; run `basemind scan --view {view}` first \
                 (or omit --view to serve the working view)"
            );
        }
    }
    // Client-side pre-flight: the daemon pool refuses the same roots, but failing here yields the
    // full guidance on the client's own stderr AND protects a new client talking to an older daemon.
    guard_workspace_root(root)?;

    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        // No in-process fallback: relaying is the only path. If it fails the client sees a clear
        // error rather than silently degrading to a fragile pipe-bound server.
        try_serve_relay(root, view).context(
            "relay to the basemind daemon failed; run `basemind daemon ensure` to check the daemon, \
             then retry",
        )
    }
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    {
        let _ = root;
        anyhow::bail!(
            "`basemind serve` needs the comms daemon to host the MCP server; rebuild with \
             `--features comms` (the stdio MCP server is served by relaying to that daemon)."
        )
    }
}

/// Keep the MCP host's stdio pipes alive across daemon replacement. The protocol-aware relay tracks
/// initialization and in-flight request IDs, allowing it to reconnect without asking Codex or
/// Claude to recreate the MCP process.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn try_serve_relay(root: &std::path::Path, view: &str) -> Result<()> {
    use basemind::comms::identity;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let agent = identity::mcp_session_agent_id(root);
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let view = view.to_owned();
        let stream = open_relay_connection(canonical_root.clone(), view.clone(), agent.clone()).await?;

        tracing::info!(
            pid = std::process::id(),
            view = %view,
            root = %canonical_root.display(),
            "basemind serve: persistent relay connected"
        );
        stdio_relay::run(tokio::io::stdin(), tokio::io::stdout(), stream, move || {
            open_relay_connection(canonical_root.clone(), view.clone(), agent.clone())
        })
        .await
        .context("run persistent daemon relay")
    })
}

#[cfg(all(feature = "comms", unix))]
type RelayStream = tokio::net::UnixStream;

#[cfg(all(feature = "comms", windows))]
type RelayStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(all(feature = "comms", any(unix, windows)))]
async fn open_relay_connection(
    root: PathBuf,
    view: String,
    agent: basemind::comms::ids::AgentId,
) -> Result<RelayStream> {
    use basemind::comms::{relay, singleton};

    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    singleton::ensure_daemon(&paths).await.context("ensure daemon")?;
    let mut stream = relay_connect_stream(&paths.socket_path)
        .await
        .context("connect to daemon socket")?;
    let hello = relay::RelayHello {
        relay_proto_ver: relay::RELAY_PROTO_VER,
        root,
        view,
        agent,
    };
    let welcome = relay::client_handshake(&mut stream, &hello)
        .await
        .context("relay handshake")?;
    if welcome.relay_proto_ver != relay::RELAY_PROTO_VER {
        anyhow::bail!(
            "daemon relay-proto {} != client {}",
            welcome.relay_proto_ver,
            relay::RELAY_PROTO_VER
        );
    }
    if !welcome.accepted {
        anyhow::bail!("daemon declined relay: {:?}", welcome.code);
    }
    Ok(stream)
}

/// Dial the daemon's relay endpoint: a Unix-domain socket on unix.
#[cfg(all(feature = "comms", unix))]
async fn relay_connect_stream(socket_path: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(socket_path).await
}

/// Dial the daemon's relay endpoint: a named pipe on Windows, retrying while the pipe is busy.
#[cfg(all(feature = "comms", windows))]
async fn relay_connect_stream(
    socket_path: &std::path::Path,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_PIPE_BUSY: i32 = 231;
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match ClientOptions::new().open(socket_path) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            Err(source) => return Err(source),
        }
    }
}

fn cmd_hook_install(root: &std::path::Path) -> Result<()> {
    let hooks_dir = root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        anyhow::bail!("no .git/hooks directory at {}", hooks_dir.display());
    }
    let hook_path = hooks_dir.join("pre-commit");
    let body = r#"#!/usr/bin/env sh
# Installed by basemind hook install.
set -e
exec basemind scan --staged --quiet
"#;
    std::fs::write(&hook_path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    println!("installed pre-commit hook at {}", hook_path.display());
    Ok(())
}
