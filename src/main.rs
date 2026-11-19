mod main_rescan;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod main_scan_forward;
use main_rescan::cmd_rescan;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use basemind::config::overrides::DocumentsCliOverrides;
use basemind::config::{self, Config};
use basemind::render::{self, Verbosity};
use basemind::store::{LockHolder, Store};
use basemind::watcher::{BatchKind, WatchBatch};

mod comms_cli;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod comms_stop;
mod doctor_cli;
mod lang_cli;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod relay_refusal;
#[cfg(all(feature = "comms", unix))]
mod serve_lead;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod serve_relay;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod stdio_relay;

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

    /// Emit machine-readable JSON instead of the human-readable rendering. Applies to the tool
    /// subcommands (code / git / memory / admin / status / cache), the
    /// coordination groups (agent / workspace / comms / daemon), doctor and sessions. It is ignored, with a
    /// warning, on init / scan / rescan / watch / hook / lang / completions / man / statusline /
    /// serve.
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
    /// Run a one-shot scan over the repository and write the code map. With a basemind daemon
    /// running, a working-tree scan is forwarded to it, the workspace's writer.
    Scan(ScanArgs),
    /// Re-index the working tree (full) or only the given paths (incremental). Use after
    /// edits, or to rebuild a stale/empty index without starting the server.
    Rescan(RescanArgs),
    /// Long-running watcher; keeps the code map current as files change.
    Watch,
    /// Read the code map: outline, symbols, read, grep, search, refs, find (plus shorthands).
    #[command(subcommand)]
    Code(basemind::cli::code::CodeCmd),
    /// Git history / blame / diff queries.
    #[command(subcommand)]
    Git(basemind::cli::git::GitCmd),
    /// Shared agent memory + document search (needs `--features memory,documents`).
    #[command(subcommand)]
    Memory(basemind::cli::memory::MemoryCmd),
    /// Repository identity, telemetry, token counting and retrieval evaluation.
    #[command(subcommand)]
    Admin(basemind::cli::admin::AdminCmd),
    /// Manage the git pre-commit hook that runs `basemind scan --staged`.
    Hook {
        #[command(subcommand)]
        action: basemind::cli::hook::HookCmd,
    },
    /// Check this installation and workspace: root, config, index, grammars, hook, daemon. Exits 1
    /// when a check fails.
    Doctor,
    /// Index health for this workspace: file counts, languages, scan age.
    Status,
    /// Print a shell completion script to stdout (`basemind completions zsh > _basemind`).
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout (`basemind man > basemind.1`).
    Man,
    /// Manage downloaded tree-sitter grammars.
    Lang {
        #[command(subcommand)]
        action: LangCmd,
    },
    /// Run an MCP server for a stdio client: ensure the daemon (the real server) is up, then relay
    /// this process's stdin/stdout to it. HTTP-native clients skip this and dial the daemon URL
    /// directly (see `daemon ensure`).
    Serve {
        /// Host the agent-launch supervisor in this process (ADR-0040): answer `agent_launch` and
        /// `agent_stop` on this stdio, as the operator's user-level agents.toml allows. Without
        /// that file launching stays off and the tools are not listed. Unix only.
        #[arg(long)]
        lead: bool,
    },
    /// Print a compact one-line summary of the daemon's currently-hot workspaces, for a shell
    /// statusline. Fast and silent: prints nothing and exits 0 when no daemon is running.
    Statusline,
    /// Tool-use counts from the local Claude Code and Codex transcripts of this repository and its
    /// worktrees: basemind calls against shell searches and whole-file reads. Counts and paths only.
    Sessions(basemind::cli::sessions::SessionsArgs),
    /// Manage the machine-global caches (gc / stats / clear). Offline path.
    #[command(subcommand)]
    Cache(basemind::cli::admin::CacheCmd),
    /// Manage the user-global agent-comms broker daemon (needs `--features comms`).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Comms {
        #[command(subcommand)]
        action: CommsLifecycleCmd,
    },
    /// Agent messaging: post, read and manage threads with the other agents of this repository
    /// (needs `--features comms`). Mirrors the `agent_post`, `agent_inbox` and `agent_threads` MCP
    /// tools, plus `status`, `cleanup` and the hooks' `hook`.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Agent {
        #[command(subcommand)]
        action: basemind::cli::agent::AgentCmd,
    },
    /// Machine-registry listings: workspaces / worktrees / branches (needs `--features comms`).
    /// Talks to the broker daemon directly, like `comms`.
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
/// messaging commands live in their own group, `basemind agent` (see `cli::agent`).
#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Subcommand, Debug)]
enum CommsLifecycleCmd {
    /// Run the broker loop: bind the singleton socket, serve front-ends, block until shutdown.
    Daemon,
    /// Ensure the daemon is running (spawn if needed); noop when already alive.
    Start,
    /// Ask the running daemon to drain and stop. With `--all`, stop EVERY live daemon of this
    /// version's protocol generation on this machine, not just the one for the current comms dir.
    Stop {
        /// Stop every live daemon of this protocol generation on this machine, not just the current one.
        #[arg(long)]
        all: bool,
    },
    /// Report the daemon's pid / version / uptime / room + subscriber counts.
    Status,
    /// List every live daemon of this version's protocol generation on this machine (pid / comms
    /// dir / version / uptime) and flag any pile-up over the ceiling. Prunes dead registry entries
    /// as a side effect. Reports process liveness only unless --probe is given.
    Doctor {
        /// Also ask each comms daemon whether it can actually serve, and report the verdict per
        /// row. Off by default so the plain report stays RPC-free and safe on a wedged machine.
        #[arg(long)]
        probe: bool,
        /// Acknowledge and delete the records of scans whose process died without unwinding.
        #[arg(long)]
        clear_fatal: bool,
    },
    /// Remove rendezvous directories no daemon uses: other protocol generations' and the files
    /// releases up to 0.33 left in the comms directory, when unlocked and idle for 30 days.
    Cleanup {
        /// List what would be removed, with sizes, and remove nothing.
        #[arg(long)]
        dry_run: bool,
        /// Idle days after which an unlocked rendezvous is removed. With 0, the comms store of
        /// another release, messages included, is removed as soon as no daemon of it is running.
        #[arg(long, value_name = "DAYS", default_value_t = 30)]
        older_than: u64,
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
    /// Written as the `rev-<sha7>` view in the machine-global workspace cache, separate from the
    /// working-tree view.
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
    /// Repo-relative paths to re-index incrementally. When omitted the entire working tree is
    /// re-indexed, skipping unchanged files. Paths are forward-slash with no leading `/`.
    #[arg(value_name = "PATH")]
    paths: Vec<String>,
    /// Skip building the git-history index after the rescan (overrides config).
    #[arg(long)]
    no_git_history: bool,
    /// Wipe and fully rebuild the git-history index instead of incrementally syncing it.
    #[arg(long)]
    rebuild_git_history: bool,
}

#[derive(Subcommand, Debug)]
enum LangCmd {
    /// Show installed grammars and where they live.
    List,
    /// Force-download all supported grammars, plus any `[languages.<name>] preload = true` from
    /// basemind.toml (no-op if already cached).
    Install,
    /// Delete the grammar cache. Next run will redownload.
    Clean,
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

// mimalloc (why: `basemind::allocator`); the leak-check build (`system-allocator`) keeps the system
// allocator so `leaks` and valgrind can scan the whole heap.
#[cfg(not(feature = "system-allocator"))]
#[global_allocator]
static GLOBAL: basemind::allocator::MiMalloc = basemind::allocator::MiMalloc;

/// Exit-code contract: see [`basemind::cli::exit`].
fn main() -> std::process::ExitCode {
    // First, before argument parsing, logging or the runtime: a lead seals itself so no process of
    // the same user can write requests into its stdin through /proc in the meantime.
    #[cfg(all(feature = "comms", unix))]
    serve_lead::seal_if_lead();
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {}", basemind::cli::exit::render(&error));
            std::process::ExitCode::from(basemind::cli::exit::exit_code_for(&error))
        }
    }
}

fn run() -> Result<()> {
    basemind::alloc_tag::retag_heap_pages();
    let process_started = std::time::Instant::now();
    // ~keep Parsed by hand so `scan` can tell a flag the user typed from a default or env value.
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let verbosity = Verbosity::from_flags(cli.quiet, cli.verbose);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_log_directive(verbosity))),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    // Before any command touches the data home: every layer below reads this, not the environment.
    // The status line and the hooks run on every prompt and tool call; their output is not the
    // place for a misset variable's warning, which every other command prints. ~keep
    let _engine = if matches!(cli.cmd, Cmd::Statusline | Cmd::Hook { .. }) {
        basemind::cli::env_config::install_from_env_quietly()?
    } else {
        basemind::cli::env_config::install_from_env()?
    };

    // Every process that may build an ONNX session bounds its memory before the first one exists. The
    // comms daemon applies its own (tighter) thread cap in `comms_daemon::run`, and a one-shot scan its
    // own (wider) one in `cmd_scan` once its config is loaded; both must win.
    #[cfg(feature = "intelligence")]
    {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        let is_comms = matches!(cli.cmd, Cmd::Comms { .. });
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        let is_comms = false;
        if !is_comms && !matches!(cli.cmd, Cmd::Scan(_)) {
            basemind::embeddings::bound_ort_memory(basemind::embeddings::ort_intra_threads(
                basemind::embeddings::EmbedProcess::Interactive,
                0,
                rayon::current_num_threads(),
            ));
        }
    }

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
            from = "ancestor basemind.toml",
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
        Cmd::Scan(args) => {
            let scan_matches = matches.subcommand_matches("scan").unwrap_or(&matches);
            cmd_scan(&root, &args, scan_matches, verbosity, no_color)
        }
        Cmd::Rescan(args) => cmd_rescan(&root, &args, verbosity, no_color),
        Cmd::Watch => cmd_watch(&root, verbosity, no_color),
        Cmd::Code(c) => {
            let _ = basemind::lang::ensure_grammars();
            dispatch(basemind::cli::ToolCmd::Code(c))
        }
        Cmd::Git(g) => dispatch(basemind::cli::ToolCmd::Git(g)),
        Cmd::Memory(m) => dispatch(basemind::cli::ToolCmd::Memory(m)),
        Cmd::Admin(a) => dispatch(basemind::cli::ToolCmd::Admin(a)),
        Cmd::Hook { action } => basemind::cli::hook::run(&root, action),
        Cmd::Doctor => doctor_cli::cmd_doctor(&root, json),
        Cmd::Status => {
            let _ = basemind::lang::ensure_grammars();
            dispatch(basemind::cli::ToolCmd::Status)
        }
        Cmd::Completions { shell } => {
            // Buffered so a closed pipe (`| head`) is a quiet error, not clap_complete's write panic.
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "basemind", &mut script);
            std::io::Write::write_all(&mut std::io::stdout(), &script).map_err(Into::into)
        }
        Cmd::Man => clap_mangen::Man::new(Cli::command())
            .render(&mut std::io::stdout())
            .map_err(Into::into),
        Cmd::Lang { action } => match action {
            LangCmd::List => lang_cli::cmd_lang_list(no_color),
            LangCmd::Install => lang_cli::cmd_lang_install(&root, verbosity, no_color),
            LangCmd::Clean => lang_cli::cmd_lang_clean(),
        },
        Cmd::Serve { lead } => cmd_serve(&root, &view, lead),
        Cmd::Cache(action) => basemind::cli::run_cache(&root, action, json),
        Cmd::Sessions(args) => basemind::cli::sessions::run(&root, json, &args),
        // An explicit `--root` selects the per-repo line for that (resolved) workspace; bare
        // `basemind statusline` keeps the daemon hot-workspace summary.
        Cmd::Statusline => comms_cli::cmd_statusline(cli.root.as_ref().map(|_| root.as_path())),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Comms { action } => comms_cli::cmd_comms(action, json),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Agent { action } => basemind::cli::agent::run(&root, json, action),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Workspace { action } => basemind::cli::registry::run(json, action),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Daemon { action } => comms_cli::cmd_daemon(action, json),
    }
}

/// Emit a `WARN` when a global flag was supplied to a subcommand that does not
/// consume it. `--json` only affects the tool subcommands and the coordination groups;
/// `--view` additionally affects `serve`. Everything else
/// ignores them, so warning prevents a no-op flag from looking effective.
fn warn_ignored_global_flags(cmd: &Cmd, json: bool, view: &str) {
    let consumes_json = matches!(
        cmd,
        Cmd::Code(_) | Cmd::Git(_) | Cmd::Memory(_) | Cmd::Admin(_) | Cmd::Cache(_) | Cmd::Status | Cmd::Doctor
    );
    #[cfg(all(feature = "comms", any(unix, windows)))]
    let consumes_json = consumes_json
        || matches!(
            cmd,
            Cmd::Comms { .. } | Cmd::Agent { .. } | Cmd::Workspace { .. } | Cmd::Daemon { .. }
        );
    let consumes_view = consumes_json || matches!(cmd, Cmd::Serve { .. });
    let consumes_json = consumes_json || matches!(cmd, Cmd::Sessions(_));

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

/// Refuse a workspace root that is not a project (issue #62), returning the RESOLVED root the
/// caller must use from here on. Root discovery still falls back to the start directory, so a host
/// launched at `/` (or any stray directory) resolves to a root that basemind would open read-write
/// and walk in full. Every verb that opens a root for indexing — and `serve`, which does it a hop
/// away in the daemon — gates on this first, so the operator gets the guidance instead of an OOM.
///
/// Rebinding `root` to the returned path is load-bearing, not cosmetic: it is what guarantees the
/// path that was checked is the path that gets opened, closing the check-one-path-open-another gap
/// that let `/..` past a purely syntactic filesystem-root test.
fn guard_workspace_root(root: &std::path::Path) -> Result<PathBuf> {
    config::root_guard::workspace_root_verdict(root)
        .map_err(|refusal| anyhow::anyhow!(config::root_guard::refusal_message(root, refusal)))
}

/// Variant of [`load_or_default`] that also applies a CLI override layer through
/// the layered merger. Used by `scan` / `serve` to flow `#[command(flatten)]`
/// flags down to the resolved config.
fn load_or_default_with(root: &std::path::Path, cli: Option<DocumentsCliOverrides>) -> Result<Config> {
    match config::overrides::load_with_overrides(root, None, cli) {
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
             running this directly is unnecessary and would collide with it. Use `basemind rescan` \
             (forwarded to the daemon when one is running) to refresh the index, or stop it first.",
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
    if force_rebuild && let Err(error) = index.clear() {
        tracing::warn!(?error, "git-history index clear failed");
    }
    match basemind::git_history::builder::sync(&index, &repo, &basemind_dir) {
        Ok(outcome) => {
            let summary = match outcome {
                basemind::git_history::builder::RebuildOutcome::Fresh => "git-history index: up to date".to_string(),
                basemind::git_history::builder::RebuildOutcome::Skipped { reason } => {
                    format!("git-history index: not updated ({reason})")
                }
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
}

/// Run a full scan, printing each file's line as the scan absorbs it.
///
/// The observer owns its own stdout stream, so it never collides with the caller's `out` and the
/// CLI never holds a per-file result for the whole repo just to print one line each. The caller
/// renders the summary from `report.stats` afterwards.
fn scan_streaming(
    no_color: bool,
    verbosity: Verbosity,
    source: basemind::scanner::ScanSource<'_>,
    root: &std::path::Path,
    store: &mut Store,
    config: &basemind::config::Config,
    cancel: &basemind::scanner::ScanCancel,
) -> std::result::Result<basemind::scanner::ScanReport, basemind::scanner::ScanError> {
    // The pass releases the embedding engine it loaded when the scan ends, so the model is not held
    // through the git-history sync and until exit.
    #[cfg(feature = "intelligence")]
    let _embed_pass = basemind::embeddings::begin_embed_pass();
    let mut observer = render::ScanLineObserver::new(no_color, verbosity);
    let report = basemind::scanner::scan_with_observer(
        root,
        store,
        config,
        source,
        basemind::scanner::EmbedMode::Inline,
        cancel,
        &mut observer,
        basemind::retrieval_wiring::backend(),
    );
    observer.finish();
    report
}

/// Turn an interrupted scan into the exit-`130` error after printing what did land. The scan stops
/// at a file boundary with its completed batches committed and the stale purge skipped, so the
/// index is consistent but incomplete — the message says to rerun.
fn interrupted(
    out: &mut anstream::AutoStream<std::io::Stdout>,
    report: &basemind::scanner::ScanReport,
    verbosity: Verbosity,
    what: &str,
) -> Result<()> {
    render::render_summary(out, &report.stats, verbosity);
    Err(basemind::cli::exit::CliExit::interrupted(format!(
        "{what} interrupted: files already indexed are committed and the index is consistent but incomplete; \
         rerun to finish"
    )))
}

fn cmd_scan(
    root: &std::path::Path,
    args: &ScanArgs,
    matches: &clap::ArgMatches,
    verbosity: Verbosity,
    no_color: bool,
) -> Result<()> {
    // ~keep Installed first: from here on Ctrl-C asks the scan to stop cleanly instead of killing the process.
    let cancel = basemind::cli::interrupt::install();
    let root = &guard_workspace_root(root)?;
    let mut out = render::stdout(no_color);
    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        let view_flag = if args.staged {
            Some("--staged")
        } else {
            args.rev.as_ref().map(|_| "--rev")
        };
        match view_flag {
            Some(flag) => main_scan_forward::refuse_view_scan_under_daemon(root, flag)?,
            None if main_scan_forward::forward_scan(root, args, matches, &cancel, &mut out)? => return Ok(()),
            None => {}
        }
    }
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    let _ = matches;
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default_with(root, Some(args.documents.clone()))?;
    #[cfg(feature = "intelligence")]
    bound_scan_ort_threads(&config);

    if args.staged {
        let repo = basemind::git::Repo::discover(root).context("`--staged` requires being inside a git repository")?;
        let mut store = open_store_for_write(root, basemind::store::VIEW_STAGED, "staged", LockHolder::Scan)?;
        render::render_scan_header(&mut out, "staged index", verbosity);
        let report = scan_streaming(
            no_color,
            verbosity,
            basemind::scanner::ScanSource::Staged(&repo),
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("scan staged")?;
        if report.cancelled {
            return interrupted(&mut out, &report, verbosity, "scan --staged");
        }
        render::render_summary(&mut out, &report.stats, verbosity);
        render::render_phase_peaks(&mut out, &report.phase_peaks, verbosity);
        return Ok(());
    }
    if let Some(rev_spec) = &args.rev {
        let repo = basemind::git::Repo::discover(root).context("`--rev` requires being inside a git repository")?;
        let sha = repo.resolve_rev(rev_spec).context("resolve rev")?;
        let short = &sha[..7.min(sha.len())];
        let view = basemind::store::view_name_for_rev(short);
        let mut store = open_store_for_write(root, &view, "rev", LockHolder::Scan)?;
        render::render_scan_header(&mut out, &format!("rev {short}"), verbosity);
        let report = scan_streaming(
            no_color,
            verbosity,
            basemind::scanner::ScanSource::Rev {
                repo: &repo,
                sha: sha.clone(),
            },
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("scan rev")?;
        if report.cancelled {
            return interrupted(&mut out, &report, verbosity, "scan --rev");
        }
        render::render_summary(&mut out, &report.stats, verbosity);
        render::render_phase_peaks(&mut out, &report.phase_peaks, verbosity);
        return Ok(());
    }

    if let Some(notice) = writer_collision_notice(root) {
        return Err(basemind::cli::exit::CliExit::busy(notice));
    }
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "scan", LockHolder::Scan)?;
    let report = scan_streaming(
        no_color,
        verbosity,
        basemind::scanner::ScanSource::WorkingTree,
        root,
        &mut store,
        &config,
        &cancel,
    )
    .context("scan")?;
    if report.cancelled {
        return interrupted(&mut out, &report, verbosity, "scan");
    }
    render::render_summary(&mut out, &report.stats, verbosity);
    render::render_phase_peaks(&mut out, &report.phase_peaks, verbosity);
    reclaim_after_scan(&mut out, &report.stats, std::mem::take(&mut store.reset_stale_schema));
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}

/// Give a one-shot scan's ONNX sessions the cores of its scan pool (`[resources] scan_threads`, else
/// rayon's default), unless `[resources] embed_threads` names a count; see
/// [`basemind::embeddings::ort_intra_threads`].
#[cfg(feature = "intelligence")]
fn bound_scan_ort_threads(config: &basemind::config::Config) {
    let resources = &config.resources;
    let cores = match resources.scan_threads {
        0 => rayon::current_num_threads(),
        n => n,
    };
    let threads = basemind::embeddings::ort_intra_threads(
        basemind::embeddings::EmbedProcess::OneShotScan,
        resources.effective_embed_threads(config.documents.embed_max_threads),
        cores,
    );
    tracing::debug!(threads, cores, "ONNX intra-op threads for this scan");
    basemind::embeddings::bound_ort_memory(threads);
}

/// Reap what a migrating, re-extracting or schema-resetting pass orphaned. Gated on the pass having displaced anything,
/// so an ordinary no-op or edit-only scan never pays for the cross-workspace reference count.
fn reclaim_after_scan(
    out: &mut anstream::AutoStream<std::io::Stdout>,
    stats: &basemind::scanner::ScanStats,
    schema_reset: bool,
) {
    if !stats.displaced_artifacts() && !schema_reset {
        return;
    }
    match basemind::store_gc::reclaim_displaced_blobs(&basemind::data_root::DataRoot::current()) {
        Ok(report) => render::render_cleanup(out, &report),
        Err(error) => tracing::warn!(%error, "post-scan blob cleanup failed; the daemon's next sweep will retry"),
    }
}

fn cmd_watch(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    let root = &guard_workspace_root(root)?;
    bootstrap_grammars(verbosity, no_color)?;
    let config = Arc::new(load_or_default(root)?);
    let store = Arc::new(Mutex::new(
        Store::open_with_holder(root, basemind::store::VIEW_WORKING, LockHolder::Watch).context("open store")?,
    ));
    // Each watcher batch writes blobs without an fsync and leaves no breadcrumb: if the machine goes
    // down under this process, the marker asks the next scan to verify them. ~keep
    #[cfg(any(unix, windows))]
    let _hosting = basemind::scan_evidence::HostingMarker::begin();

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
                render::render_files(&mut stdout, batch.results, verbosity);
                render::render_summary(&mut stdout, &batch.report.stats, verbosity);
            }
            BatchKind::Incremental { paths } => {
                render::render_batch_header(&mut stdout, paths, verbosity);
                render::render_files(&mut stdout, batch.results, verbosity);
            }
            BatchKind::FullRescan => {
                render::render_files(&mut stdout, batch.results, verbosity);
                render::render_summary(&mut stdout, &batch.report.stats, verbosity);
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
fn cmd_serve(root: &std::path::Path, view: &str, lead: bool) -> Result<()> {
    if lead && !cfg!(unix) {
        anyhow::bail!(
            "`basemind serve --lead` is not supported on this platform: launching agents needs a Unix \
             process tree to contain them (ADR-0040)"
        );
    }
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
    // The relay carries the RESOLVED root, so the daemon opens the directory this check approved
    // rather than re-deriving one from a spelling the client chose.
    let root = &guard_workspace_root(root)?;

    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        // No in-process fallback: relaying is the only path. If it fails the client sees a clear
        // error rather than silently degrading to a fragile pipe-bound server.
        #[cfg(unix)]
        let served = if lead {
            serve_relay::try_serve_lead(root, view)
        } else {
            serve_relay::try_serve_relay(root, view)
        };
        #[cfg(not(unix))]
        let served = serve_relay::try_serve_relay(root, view);
        served.context(
            "relay to the basemind daemon failed; run `basemind daemon ensure` to check the daemon, \
             then retry",
        )
    }
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    {
        let _ = (root, lead);
        anyhow::bail!(
            "`basemind serve` needs the comms daemon to host the MCP server; rebuild with \
             `--features comms` (the stdio MCP server is served by relaying to that daemon)."
        )
    }
}

#[cfg(test)]
mod allocator_tests {
    /// The binary's heap is mimalloc's in every shipped feature set, and the system allocator's in
    /// the leak-check build: these tests link the binary's own `#[global_allocator]`.
    #[test]
    fn installs_mimalloc_unless_built_for_leak_checking() {
        let block = Box::new([7u8; 64]);
        assert_eq!(
            basemind::allocator::owned_by_mimalloc(block.as_ptr()),
            !cfg!(feature = "system-allocator")
        );
    }
}
