use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use basemind::config::{self, Config, DocumentsCliOverrides};
use basemind::render::{self, Verbosity};
use basemind::store::{LockHolder, Store};
use basemind::watcher::{BatchKind, WatchBatch};

mod lang_cli;

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
    /// Initialize a new .basemind/ folder with a default config.
    Init,
    /// Run a one-shot scan over the repository and write the code map.
    Scan(ScanArgs),
    /// Re-index the working tree (full) or only the given paths (incremental). Use after
    /// edits, or to rebuild a stale/empty index without starting the server.
    Rescan(RescanArgs),
    /// Long-running watcher; keeps the code map current as files change.
    Watch,
    /// Query the code map (outline, search, references, call-graph, …).
    #[command(subcommand)]
    Query(basemind::cli::codemap::QueryCmd),
    /// Git history / blame / diff queries.
    #[command(subcommand)]
    Git(basemind::cli::git::GitCmd),
    /// Shared agent memory + document search (needs `--features memory,documents`).
    #[command(subcommand)]
    Memory(basemind::cli::memory::MemoryCmd),
    /// Governance: mine co-change proposals, list, accept, reject (needs `--features memory`).
    #[command(subcommand)]
    Governance(basemind::cli::governance::GovernanceCmd),
    /// On-demand web ingestion (needs `--features crawl`).
    #[command(subcommand)]
    Web(basemind::cli::web::WebCmd),
    /// Aggregate telemetry into a usage summary.
    Telemetry {
        /// Aggregation window: `today` (default), `1h`, `24h`, `all`.
        #[arg(long)]
        window: Option<String>,
        /// Optional exact tool-name filter.
        #[arg(long)]
        tool: Option<String>,
    },
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
    /// Run an MCP server (stdio) exposing the code map to AI agents.
    Serve(ServeArgs),
    /// Manage the `.basemind/` caches (gc / stats / clear). Offline path.
    #[command(subcommand)]
    Cache(basemind::cli::admin::CacheCmd),
    /// Manage the user-global agent-comms broker daemon (needs `--features comms`).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Comms {
        #[command(subcommand)]
        action: CommsLifecycleCmd,
    },
    /// Run the A2A protocol server: gRPC + JSON-RPC 2.0 + agent card + SSE on one
    /// listener (needs `--features a2a`).
    #[cfg(feature = "a2a")]
    A2a {
        #[command(subcommand)]
        action: A2aCmd,
    },
}

/// Subcommands for `basemind a2a`.
#[cfg(feature = "a2a")]
#[derive(Subcommand, Debug)]
enum A2aCmd {
    /// Bind the combined gRPC + JSON-RPC + SSE listener and serve until Ctrl-C.
    Serve(A2aServeArgs),
}

#[cfg(feature = "a2a")]
#[derive(clap::Args, Debug)]
struct A2aServeArgs {
    /// Address to bind, `host:port`. Defaults to loopback. Binding a public
    /// interface (`0.0.0.0:…`) is refused unless `--token`/`--token-file` is set.
    #[arg(long, default_value = "127.0.0.1:8723")]
    addr: std::net::SocketAddr,
    /// Agent name advertised in the agent card (defaults to "basemind").
    #[arg(long)]
    name: Option<String>,
    /// Agent description advertised in the agent card.
    #[arg(long)]
    description: Option<String>,
    /// Bearer token required on every request except the public agent card.
    /// On a CLI this is visible in the process list (`ps`, `/proc/<pid>/cmdline`);
    /// prefer `--token-file`, or the `BASEMIND_A2A_TOKEN` env var, in shared
    /// environments. Takes precedence over `--token-file`.
    #[arg(long, env = "BASEMIND_A2A_TOKEN")]
    token: Option<String>,
    /// Path to a bearer-token file (auto-created with `0600` permissions when
    /// missing). Enables bearer auth.
    #[arg(long)]
    token_file: Option<std::path::PathBuf>,
    /// PEM certificate (chain) for TLS termination. Must be paired with
    /// `--tls-key`; supplying exactly one is a usage error. When both are set the
    /// server serves HTTPS and negotiates HTTP/2 (gRPC) vs HTTP/1.1 via ALPN.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<std::path::PathBuf>,
    /// PEM private key matching `--tls-cert`. Must be paired with `--tls-cert`.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<std::path::PathBuf>,
}

/// Subcommands for `basemind comms`: daemon lifecycle plus the agent verbs.
///
/// Lifecycle verbs (`Daemon`/`Start`/`Stop`/`Status`) manage the singleton broker process. The
/// agent verbs (`Register`/`Agents`/`RoomCreate`/`Rooms`/`Join`/`Leave`/`Post`/`History`/`Read`/
/// `Inbox`) connect to the daemon DIRECTLY via `CommsClient::ensure_and_connect` (see
/// `cli::comms`) — they never build a full server, so they cannot clash with a running `serve`.
#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Subcommand, Debug)]
enum CommsLifecycleCmd {
    /// Run the broker loop: bind the singleton socket, serve front-ends, block until shutdown.
    Daemon,
    /// Ensure the daemon is running (spawn if needed); noop when already alive.
    Start,
    /// Ask the running daemon to drain and stop.
    Stop,
    /// Report the daemon's pid / version / uptime / room + subscriber counts.
    Status,
    /// Agent verbs (register / rooms / post / history / inbox …) against the broker.
    #[command(flatten)]
    Agent(basemind::cli::comms::CommsAgentCmd),
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
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    // The view to serve comes from the global `--view` flag (see `Cli::view`), passed
    // into `cmd_serve` — a single source of truth so the two cannot diverge.
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
    #[cfg(all(feature = "shells", any(unix, windows)))]
    if let Some(result) = basemind::shells::intercept_internal_reexec() {
        return result;
    }
    // Parse before initializing tracing so the verbosity flag can feed the default
    // log threshold. `Cli::parse()` exits on `--help`/errors and logs nothing, so
    // running it ahead of subscriber init is safe.
    let cli = Cli::parse();
    let verbosity = Verbosity::from_flags(cli.quiet, cli.verbose);

    // Diagnostics → stderr so they never collide with subcommand output (especially `serve`,
    // whose stdout is the MCP JSON-RPC transport). An explicit `RUST_LOG` wins; otherwise the
    // default threshold tracks `--quiet` / `--verbose` so `-q` actually silences subsystem logs.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(default_log_directive(verbosity))),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // Install the rustls crypto provider once, at startup, before anything can
    // perform a TLS handshake. aws-lc-rs AND ring are both in the dependency
    // tree (via reqwest/hyper-rustls), so the process-default provider is
    // ambiguous and a later `ServerConfig::builder()` would panic; pinning it
    // here removes any ordering dependency. Idempotent — a prior install wins.
    #[cfg(feature = "a2a")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let no_color = cli.no_color;
    let start = cli
        .root
        .clone()
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    // Prefer the discovered git workdir so `basemind` invoked from a subdirectory still
    // operates against the repo root. Outside a git repo, fall back to CWD.
    let root = match basemind::git::Repo::discover(&start) {
        Ok(repo) => repo.workdir().to_path_buf(),
        Err(_) => start,
    };

    let json = cli.json;
    let view = cli.view.clone();
    // `--json` / `--view` are global for ergonomics but only the tool subcommands
    // (and `serve`, for `--view`) consume them. Warn — rather than silently ignore —
    // when they're passed to a command that has no use for them, so a typo'd
    // invocation doesn't appear to take effect.
    warn_ignored_global_flags(&cli.cmd, json, &view);
    // Query reads cached extracts; grammars are not strictly needed, but the L2
    // escalation path falls back to live extraction. Bootstrap quietly for the
    // tool subcommands so first-time L2 doesn't stall.
    //
    // `dispatch` collapses the identical `cli::run` tool arms (same root/view/json/overrides,
    // differing only in the `ToolCmd` variant) into one call site — removes the duplication and
    // keeps main.rs under the per-file line cap.
    let dispatch =
        |tc| basemind::cli::run(&root, &view, DocumentsCliOverrides::default(), json, tc);
    match cli.cmd {
        Cmd::Init => cmd_init(&root),
        Cmd::Scan(args) => cmd_scan(&root, &args, verbosity, no_color),
        Cmd::Rescan(args) => cmd_rescan(&root, &args, verbosity, no_color),
        Cmd::Watch => cmd_watch(&root, verbosity, no_color),
        Cmd::Query(q) => {
            let _ = basemind::lang::ensure_grammars();
            dispatch(basemind::cli::ToolCmd::Query(q))
        }
        Cmd::Git(g) => dispatch(basemind::cli::ToolCmd::Git(g)),
        Cmd::Memory(m) => dispatch(basemind::cli::ToolCmd::Memory(m)),
        Cmd::Governance(g) => dispatch(basemind::cli::ToolCmd::Governance(g)),
        Cmd::Web(w) => dispatch(basemind::cli::ToolCmd::Web(w)),
        Cmd::Telemetry { window, tool } => {
            dispatch(basemind::cli::ToolCmd::Telemetry { window, tool })
        }
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
        Cmd::Serve(args) => cmd_serve(&root, &view, &args),
        Cmd::Cache(action) => basemind::cli::run_cache(&root, action, json),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        Cmd::Comms { action } => cmd_comms(&root, action, json),
        #[cfg(feature = "a2a")]
        Cmd::A2a { action } => cmd_a2a(action),
    }
}

/// Dispatch a `basemind a2a` subcommand. `serve` blocks on a dedicated tokio
/// runtime inside [`basemind::a2a::run_server`] until Ctrl-C.
#[cfg(feature = "a2a")]
fn cmd_a2a(action: A2aCmd) -> Result<()> {
    match action {
        A2aCmd::Serve(args) => {
            let options = basemind::a2a::A2aServeOptions {
                addr: args.addr,
                name: args.name,
                description: args.description,
                token: args.token,
                token_file: args.token_file,
                tls_cert: args.tls_cert,
                tls_key: args.tls_key,
            };
            basemind::a2a::run_server(options).context("run A2A server")
        }
    }
}

/// Dispatch a comms lifecycle subcommand. Each command drives a small current-thread tokio
/// runtime — the broker daemon itself uses a multi-thread runtime so concurrent links don't
/// serialize.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms(root: &std::path::Path, action: CommsLifecycleCmd, json: bool) -> Result<()> {
    match action {
        CommsLifecycleCmd::Daemon => basemind::cli::comms_daemon::run(),
        CommsLifecycleCmd::Start => cmd_comms_start(),
        CommsLifecycleCmd::Stop => cmd_comms_lifecycle_rpc(CommsRpc::Stop, json),
        CommsLifecycleCmd::Status => cmd_comms_lifecycle_rpc(CommsRpc::Status, json),
        CommsLifecycleCmd::Agent(cmd) => basemind::cli::comms::run(root, json, cmd),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
enum CommsRpc {
    Stop,
    Status,
}

/// Ensure a daemon is running, spawning it detached if needed.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_start() -> Result<()> {
    use basemind::comms::singleton;
    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let socket_path = paths.socket_path.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        singleton::ensure_daemon(&paths)
            .await
            .map_err(|e| anyhow::anyhow!("ensure comms daemon: {e}"))
    })?;
    println!("comms daemon is running ({})", socket_path.display());
    Ok(())
}

/// Connect to the running daemon and issue a Stop or Status RPC.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_lifecycle_rpc(rpc: CommsRpc, json: bool) -> Result<()> {
    use basemind::comms::client::CommsClient;
    use basemind::comms::ids::AgentId;
    use basemind::comms::singleton;

    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        // A control client identifies as a fixed CLI agent; no scope context needed.
        let agent = AgentId::parse("basemind-cli").map_err(|e| anyhow::anyhow!("agent id: {e}"))?;
        let mut client = CommsClient::connect(&paths, agent, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))?;
        match rpc {
            CommsRpc::Stop => {
                client
                    .stop()
                    .await
                    .map_err(|e| anyhow::anyhow!("stop: {e}"))?;
                if json {
                    println!("{{\"stopped\":true}}");
                } else {
                    println!("comms daemon stopping");
                }
            }
            CommsRpc::Status => {
                let status = client
                    .status()
                    .await
                    .map_err(|e| anyhow::anyhow!("status: {e}"))?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&status)
                            .map_err(|e| anyhow::anyhow!("serialize status: {e}"))?
                    );
                } else {
                    println!(
                        "pid={} version={} proto={} uptime={}s rooms={} subscribers={}",
                        status.pid,
                        status.version,
                        status.proto_ver,
                        status.uptime_secs,
                        status.rooms,
                        status.subscribers,
                    );
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Emit a `WARN` when a global flag was supplied to a subcommand that does not
/// consume it. `--json` only affects the tool subcommands (query / git / memory /
/// web / telemetry / cache); `--view` additionally affects `serve`. Everything else
/// ignores them, so warning prevents a no-op flag from looking effective.
fn warn_ignored_global_flags(cmd: &Cmd, json: bool, view: &str) {
    let consumes_json = matches!(
        cmd,
        Cmd::Query(_)
            | Cmd::Git(_)
            | Cmd::Memory(_)
            | Cmd::Web(_)
            | Cmd::Telemetry { .. }
            | Cmd::Cache(_)
    );
    // Comms verbs emit JSON when `--json` is passed (each verb checks `if json`), so they
    // consume the flag too. Feature-gated to match the `Cmd::Comms` definition.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    let consumes_json = consumes_json || matches!(cmd, Cmd::Comms { .. });
    let consumes_view = consumes_json || matches!(cmd, Cmd::Serve(_));

    if json && !consumes_json {
        tracing::warn!("--json has no effect on this subcommand; ignoring");
    }
    if view != basemind::store::VIEW_WORKING && !consumes_view {
        tracing::warn!(view = %view, "--view has no effect on this subcommand; ignoring");
    }
}

fn bootstrap_grammars(verbosity: Verbosity, no_color: bool) -> Result<()> {
    let summary = basemind::lang::ensure_grammars()
        .map_err(|e| anyhow::anyhow!("grammar bootstrap failed: {e}"))?;
    let mut out = render::stdout(no_color);
    render::render_grammar_bootstrap(&mut out, &summary, verbosity);
    Ok(())
}

fn cmd_init(root: &std::path::Path) -> Result<()> {
    let dir = root.join(config::BASEMIND_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = config::config_path(root);
    if path.exists() {
        anyhow::bail!("config already exists at {}", path.display());
    }
    let default_toml = r##""$schema" = "v1"

[scan]
include = ["**/*.rs", "**/*.py", "**/*.ts", "**/*.tsx", "**/*.js", "**/*.go"]
exclude = ["**/target/**", "**/node_modules/**", "**/dist/**", "**/.venv/**", "**/.basemind/**", "**/.git/**"]
respect_gitignore = true
max_file_bytes = 2097152

[watch]
debounce_ms = 250

[cache]
file_map_lru = 256

[mcp]
transport = "stdio"
"##;
    std::fs::write(&path, default_toml).with_context(|| format!("write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn load_or_default(root: &std::path::Path) -> Result<Config> {
    load_or_default_with(root, None)
}

/// Variant of [`load_or_default`] that also applies a CLI override layer through
/// the layered merger. Used by `scan` / `serve` to flow `#[command(flatten)]`
/// flags down to the resolved config.
fn load_or_default_with(
    root: &std::path::Path,
    cli: Option<DocumentsCliOverrides>,
) -> Result<Config> {
    match config::load_with_overrides(root, None, cli) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => {
            // load_with_overrides already treats NotFound as "no toml file" via load(),
            // so this branch is reached only if a downstream call surfaces it again.
            tracing::info!("no .basemind/basemind.toml; using defaults");
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
fn open_store_for_write(
    root: &std::path::Path,
    view: &str,
    what: &str,
    holder: LockHolder,
) -> Result<Store> {
    Store::open_with_holder(root, view, holder).map_err(|err| {
        if err.is_lock_contention() {
            anyhow::Error::new(err).context(basemind::store::LOCK_CONTENTION_HELP.to_string())
        } else {
            anyhow::Error::new(err).context(format!("open store ({what})"))
        }
    })
}

fn cmd_scan(
    root: &std::path::Path,
    args: &ScanArgs,
    verbosity: Verbosity,
    no_color: bool,
) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default_with(root, Some(args.documents.clone()))?;

    // Decide view + source up front; we need the source to outlive scan, so the Repo lives
    // here. WorkingTree doesn't need a repo at all.
    let mut out = render::stdout(no_color);
    if args.staged {
        let repo = basemind::git::Repo::discover(root)
            .context("`--staged` requires being inside a git repository")?;
        let mut store = open_store_for_write(
            root,
            basemind::store::VIEW_STAGED,
            "staged",
            LockHolder::Scan,
        )?;
        render::render_scan_header(&mut out, "staged index", verbosity);
        let report = basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::Staged(&repo),
        )
        .context("scan staged")?;
        render::render_report(&mut out, &report, verbosity);
        // Per-file read/extract failures are non-fatal: the index WAS updated, so exit 0.
        // A genuine failure-to-update aborts earlier via `?` and surfaces a nonzero exit.
        return Ok(());
    }
    if let Some(rev_spec) = &args.rev {
        let repo = basemind::git::Repo::discover(root)
            .context("`--rev` requires being inside a git repository")?;
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
        )
        .context("scan rev")?;
        render::render_report(&mut out, &report, verbosity);
        // Per-file read/extract failures are non-fatal: the index WAS updated, so exit 0.
        return Ok(());
    }

    let mut store = open_store_for_write(
        root,
        basemind::store::VIEW_WORKING,
        "scan",
        LockHolder::Scan,
    )?;
    let report = basemind::scanner::scan(
        root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .context("scan")?;
    render::render_report(&mut out, &report, verbosity);
    // Per-file read/extract failures are non-fatal: the index WAS updated, so exit 0.
    // A genuine failure-to-update aborts earlier via `?` and surfaces a nonzero exit.
    Ok(())
}

fn cmd_rescan(
    root: &std::path::Path,
    args: &RescanArgs,
    verbosity: Verbosity,
    no_color: bool,
) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default(root)?;
    let mut store = open_store_for_write(
        root,
        basemind::store::VIEW_WORKING,
        "rescan",
        LockHolder::Rescan,
    )?;
    let mut out = render::stdout(no_color);

    // `--full` or no paths → full working-tree re-index. Otherwise re-index only the
    // supplied paths incrementally. `scan_paths` resolves paths against `root`, so make
    // each path absolute first (repo-relative input is the documented contract).
    let report = if args.full || args.paths.is_empty() {
        basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::WorkingTree,
        )
        .context("rescan (full)")?
    } else {
        let abs: Vec<PathBuf> = args.paths.iter().map(|p| root.join(p)).collect();
        basemind::scanner::scan_paths(root, &mut store, &config, &abs).context("rescan (paths)")?
    };
    render::render_report(&mut out, &report, verbosity);
    // Per-file read/extract failures are non-fatal: the index WAS updated, so exit 0
    // (matches `cmd_scan`; a genuine failure-to-update aborts earlier via `?`). Bug #24.
    Ok(())
}

fn cmd_watch(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = Arc::new(load_or_default(root)?);
    let store = Arc::new(Mutex::new(
        Store::open_with_holder(root, basemind::store::VIEW_WORKING, LockHolder::Watch)
            .context("open store")?,
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
        let cb: basemind::watcher::BatchCallback =
            Box::new(move |batch: WatchBatch<'_>| match batch.kind {
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

fn cmd_serve(root: &std::path::Path, view: &str, args: &ServeArgs) -> Result<()> {
    // A named, non-working view (`rev-<sha>`, `staged`) that was never scanned has no
    // index; serving it would silently fall back to an empty index (bug #18). The writer
    // `Store::open` would auto-create the view dir, so guard here BEFORE opening: refuse a
    // named view with no `index.msgpack`. The working view is exempt — it legitimately
    // starts empty and `serve` auto-scans it on first run.
    if view != basemind::store::VIEW_WORKING {
        let index_path = root
            .join(basemind::config::BASEMIND_DIR)
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
    // Open the store in writable mode so the `rescan` MCP tool can run the
    // scanner in-process. The MCP server is the canonical Fjall owner; the
    // standalone `basemind scan` / `basemind watch` CLIs intentionally fail
    // with a lock error when a server is already running against the repo.
    let store = Store::open_with_holder(root, view, LockHolder::Serve).context("open store")?;
    let basemind_dir = root.join(basemind::config::BASEMIND_DIR);
    let root_buf = root.to_path_buf();
    let config = Arc::new(load_or_default_with(root, Some(args.documents.clone()))?);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    // Open the git repo once if we're inside one; pass it to the server so the git-aware
    // tools (`working_tree_status`, `recent_changes`, …) work without re-discovering.
    let repo = basemind::git::Repo::discover(root).ok().map(Arc::new);
    let git_cache = Arc::new(
        basemind::git_cache::GitCache::open(
            &basemind_dir,
            args.git_cache_mem,
            !args.no_git_cache_disk,
        )
        .context("open git cache")?,
    );

    let options = basemind::mcp::ServerOptions {
        background: true,
        watch: !args.no_watch,
    };
    // Lifecycle logging (stderr → the MCP client's server logs) so a serve that "fails for some
    // reason" leaves a diagnosable trace: who started, against what, and exactly why it exited.
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        view,
        root = %root.display(),
        "basemind serve: MCP server starting"
    );
    let outcome = runtime.block_on(async move {
        use rmcp::ServiceExt;
        let server = basemind::mcp::BasemindServer::new_with_options(
            store, root_buf, config, repo, git_cache, options,
        );
        let transport = rmcp::transport::stdio();
        let service = server
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("rmcp serve: {e}"))?;
        // Block until the client disconnects (stdio EOF) or we're killed.
        service
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("rmcp waiting: {e}"))?;
        Ok::<(), anyhow::Error>(())
    });
    match &outcome {
        // Clean shutdown is the normal exit: the MCP client closed the stdio transport. Restarting
        // a stdio server is the client's responsibility — a new process can't resume the client's
        // initialize handshake — so we exit cleanly and let the client relaunch on its next call.
        Ok(()) => tracing::info!(
            pid = std::process::id(),
            "basemind serve: client disconnected, exiting"
        ),
        Err(error) => {
            tracing::error!(pid = std::process::id(), %error, "basemind serve: exiting on error")
        }
    }
    outcome
}

fn cmd_hook_install(root: &std::path::Path) -> Result<()> {
    let hooks_dir = root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        anyhow::bail!("no .git/hooks directory at {}", hooks_dir.display());
    }
    let hook_path = hooks_dir.join("pre-commit");
    // --staged makes the hook index the about-to-be-committed snapshot rather than
    // whatever messy state the working tree might be in. --quiet keeps successful commits
    // free of noise.
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
