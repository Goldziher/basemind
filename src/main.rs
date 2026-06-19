use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use basemind::config::{self, Config, DocumentsCliOverrides};
use basemind::render::{self, Verbosity};
use basemind::store::Store;
use basemind::watcher::{BatchKind, WatchBatch};

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
    /// is ignored — with a warning — on init / scan / watch / hook / lang.
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
    /// Run an MCP server (stdio) exposing the code map to AI agents.
    Serve(ServeArgs),
    /// Manage the `.basemind/` caches (gc / stats / clear). Offline path.
    #[command(subcommand)]
    Cache(basemind::cli::admin::CacheCmd),
    /// Manage the user-global agent-comms broker daemon (needs `--features comms`).
    #[cfg(feature = "comms")]
    Comms {
        #[command(subcommand)]
        action: CommsLifecycleCmd,
    },
}

/// Subcommands for `basemind comms`: daemon lifecycle plus the agent verbs.
///
/// Lifecycle verbs (`Daemon`/`Start`/`Stop`/`Status`) manage the singleton broker process. The
/// agent verbs (`Register`/`Agents`/`RoomCreate`/`Rooms`/`Join`/`Leave`/`Post`/`History`/`Read`/
/// `Inbox`) connect to the daemon DIRECTLY via `CommsClient::ensure_and_connect` (see
/// `cli::comms`) — they never build a full server, so they cannot clash with a running `serve`.
#[cfg(feature = "comms")]
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

fn main() -> Result<()> {
    // Diagnostics → stderr so they never collide with subcommand output (especially `serve`,
    // whose stdout is the MCP JSON-RPC transport).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let verbosity = Verbosity::from_flags(cli.quiet, cli.verbose);
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
    match cli.cmd {
        Cmd::Init => cmd_init(&root),
        Cmd::Scan(args) => cmd_scan(&root, &args, verbosity, no_color),
        Cmd::Watch => cmd_watch(&root, verbosity, no_color),
        Cmd::Query(q) => {
            let _ = basemind::lang::ensure_grammars();
            basemind::cli::run(
                &root,
                &view,
                DocumentsCliOverrides::default(),
                json,
                basemind::cli::ToolCmd::Query(q),
            )
        }
        Cmd::Git(g) => basemind::cli::run(
            &root,
            &view,
            DocumentsCliOverrides::default(),
            json,
            basemind::cli::ToolCmd::Git(g),
        ),
        Cmd::Memory(m) => basemind::cli::run(
            &root,
            &view,
            DocumentsCliOverrides::default(),
            json,
            basemind::cli::ToolCmd::Memory(m),
        ),
        Cmd::Web(w) => basemind::cli::run(
            &root,
            &view,
            DocumentsCliOverrides::default(),
            json,
            basemind::cli::ToolCmd::Web(w),
        ),
        Cmd::Telemetry { window, tool } => basemind::cli::run(
            &root,
            &view,
            DocumentsCliOverrides::default(),
            json,
            basemind::cli::ToolCmd::Telemetry { window, tool },
        ),
        Cmd::Hook { action } => match action {
            HookCmd::Install => cmd_hook_install(&root),
        },
        Cmd::Lang { action } => match action {
            LangCmd::List => cmd_lang_list(no_color),
            LangCmd::Install => cmd_lang_install(verbosity, no_color),
            LangCmd::Clean => cmd_lang_clean(),
        },
        Cmd::Serve(args) => cmd_serve(&root, &view, &args),
        Cmd::Cache(action) => basemind::cli::run_cache(&root, action, json),
        #[cfg(feature = "comms")]
        Cmd::Comms { action } => cmd_comms(&root, action, json),
    }
}

/// Dispatch a comms lifecycle subcommand. Each command drives a small current-thread tokio
/// runtime — the broker daemon itself uses a multi-thread runtime so concurrent links don't
/// serialize.
#[cfg(feature = "comms")]
fn cmd_comms(root: &std::path::Path, action: CommsLifecycleCmd, json: bool) -> Result<()> {
    match action {
        CommsLifecycleCmd::Daemon => cmd_comms_daemon(),
        CommsLifecycleCmd::Start => cmd_comms_start(),
        CommsLifecycleCmd::Stop => cmd_comms_lifecycle_rpc(CommsRpc::Stop, json),
        CommsLifecycleCmd::Status => cmd_comms_lifecycle_rpc(CommsRpc::Status, json),
        CommsLifecycleCmd::Agent(cmd) => basemind::cli::comms::run(root, json, cmd),
    }
}

#[cfg(feature = "comms")]
enum CommsRpc {
    Stop,
    Status,
}

/// Run the broker loop. Binds the singleton socket (the bind IS the lock), opens the store,
/// serves the Unix-socket front-end, and blocks until SIGTERM / Ctrl-C / a `Stop` RPC.
#[cfg(feature = "comms")]
fn cmd_comms_daemon() -> Result<()> {
    use std::sync::Arc;

    use basemind::comms::daemon::Broker;
    use basemind::comms::singleton;
    use basemind::comms::store::CommsStore;

    let paths = singleton::resolve_paths().context("resolve comms paths")?;

    // Bind first — the bind is the singleton lock. Probe before reclaiming a stale socket.
    let listener = match singleton::bind_listener(&paths.socket_path, singleton::probe_alive) {
        Ok(listener) => listener,
        Err(basemind::comms::singleton::SingletonError::AlreadyRunning(p)) => {
            tracing::info!(socket = %p.display(), "comms daemon already running; exiting");
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("bind comms socket: {e}")),
    };

    let store = Arc::new(CommsStore::open(&paths.comms_dir).context("open comms store")?);
    let broker = Arc::new(Broker::new(store));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Signal handling: SIGTERM / Ctrl-C begins the drain.
        let broker_for_signal = broker.clone();
        let shutdown_for_signal = shutdown_tx.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("comms: shutdown signal received; draining");
            broker_for_signal.begin_drain().await;
            let _ = shutdown_for_signal.send(true);
        });

        let frontend: Box<dyn CommsFrontendObj> = Box::new(UdsFrontendBox(
            basemind::comms::frontend_uds::UdsFrontend::from_listener(
                listener,
                paths.socket_path.clone(),
            ),
        ));
        frontend.serve_obj(broker, shutdown_rx).await
    })?;
    Ok(())
}

// `CommsFrontend::serve` uses RPITIT and is not object-safe, so wrap it behind a tiny
// object-safe shim for the single dynamic dispatch in the daemon entry point.
#[cfg(feature = "comms")]
trait CommsFrontendObj: Send {
    fn serve_obj(
        self: Box<Self>,
        broker: std::sync::Arc<basemind::comms::daemon::Broker>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;
}

#[cfg(feature = "comms")]
struct UdsFrontendBox(basemind::comms::frontend_uds::UdsFrontend);

#[cfg(feature = "comms")]
impl CommsFrontendObj for UdsFrontendBox {
    fn serve_obj(
        self: Box<Self>,
        broker: std::sync::Arc<basemind::comms::daemon::Broker>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> {
        use basemind::comms::transport::CommsFrontend;
        Box::pin(async move { Box::new(self.0).serve(broker, shutdown).await })
    }
}

/// Block until SIGTERM or Ctrl-C.
#[cfg(all(feature = "comms", unix))]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(all(feature = "comms", not(unix)))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Ensure a daemon is running, spawning it detached if needed.
#[cfg(feature = "comms")]
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
#[cfg(feature = "comms")]
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
        let mut store =
            Store::open(root, basemind::store::VIEW_STAGED).context("open store (staged)")?;
        render::render_scan_header(&mut out, "staged index", verbosity);
        let report = basemind::scanner::scan(
            root,
            &mut store,
            &config,
            basemind::scanner::ScanSource::Staged(&repo),
        )
        .context("scan staged")?;
        render::render_report(&mut out, &report, verbosity);
        if report.stats.read_failed + report.stats.extract_failed > 0 {
            std::process::exit(2);
        }
        return Ok(());
    }
    if let Some(rev_spec) = &args.rev {
        let repo = basemind::git::Repo::discover(root)
            .context("`--rev` requires being inside a git repository")?;
        let sha = repo.resolve_rev(rev_spec).context("resolve rev")?;
        let short = &sha[..7.min(sha.len())];
        let view = basemind::store::view_name_for_rev(short);
        let mut store = Store::open(root, &view).context("open store (rev)")?;
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
        if report.stats.read_failed + report.stats.extract_failed > 0 {
            std::process::exit(2);
        }
        return Ok(());
    }

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).context("open store")?;
    let report = basemind::scanner::scan(
        root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .context("scan")?;
    render::render_report(&mut out, &report, verbosity);
    if report.stats.read_failed + report.stats.extract_failed > 0 {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_watch(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = Arc::new(load_or_default(root)?);
    let store = Arc::new(Mutex::new(
        Store::open(root, basemind::store::VIEW_WORKING).context("open store")?,
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
    // Open the store in writable mode so the `rescan` MCP tool can run the
    // scanner in-process. The MCP server is the canonical Fjall owner; the
    // standalone `basemind scan` / `basemind watch` CLIs intentionally fail
    // with a lock error when a server is already running against the repo.
    let store = Store::open(root, view).context("open store")?;
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
    runtime.block_on(async move {
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
    })
}

fn cmd_lang_list(no_color: bool) -> Result<()> {
    use anstyle::{AnsiColor, Color, Reset, Style};
    use std::io::Write;
    let mut out = render::stdout(no_color);
    let installed = basemind::lang::downloaded_languages();
    let supported: std::collections::HashSet<&str> = basemind::lang::SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .collect();
    let installed_set: std::collections::HashSet<&str> =
        installed.iter().map(String::as_str).collect();

    let ok = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let warn = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    let dim = Style::new().dimmed();

    let _ = writeln!(out, "supported by basemind (queries shipped):");
    for &name in basemind::lang::SUPPORTED_LANGUAGES {
        let (sym, label, style) = if installed_set.contains(name) {
            ('✓', "ready", ok)
        } else {
            ('·', "missing", warn)
        };
        let _ = writeln!(
            out,
            "  {s}{sym} {label:<7}{r} {name}",
            s = style.render(),
            r = Reset.render(),
            sym = sym,
            label = label,
            name = name,
        );
    }

    let extras: Vec<&str> = installed
        .iter()
        .map(String::as_str)
        .filter(|n| !supported.contains(n))
        .collect();
    if !extras.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{d}also cached (no basemind queries, parse-only):{r}",
            d = dim.render(),
            r = Reset.render(),
        );
        for n in extras {
            let _ = writeln!(
                out,
                "  {d}· {n}{r}",
                d = dim.render(),
                r = Reset.render(),
                n = n,
            );
        }
    }

    if let Some(dir) = basemind::lang::grammar_cache_dir() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{d}cache: {dir}{r}",
            d = dim.render(),
            r = Reset.render(),
            dir = dir.display(),
        );
    }
    Ok(())
}

fn cmd_lang_install(verbosity: Verbosity, no_color: bool) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    if verbosity != Verbosity::Quiet {
        let summary = basemind::lang::ensure_grammars().map_err(|e| anyhow::anyhow!("{e}"))?;
        if !summary.did_download() {
            println!(
                "all {} supported grammars already cached",
                summary.already_cached.len()
            );
        }
    }
    Ok(())
}

fn cmd_lang_clean() -> Result<()> {
    basemind::lang::clean_grammar_cache().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("grammar cache cleared");
    Ok(())
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
