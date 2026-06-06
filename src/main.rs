use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use gitmind::config::{self, Config};
use gitmind::extract::SymbolKind;
use gitmind::render::{self, Verbosity};
use gitmind::store::Store;
use gitmind::watcher::{BatchKind, WatchBatch};

#[derive(Parser, Debug)]
#[command(
    name = "gitmind",
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

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialize a new .gitmind/ folder with a default config.
    Init,
    /// Run a one-shot scan over the repository and write the code map.
    Scan,
    /// Long-running watcher; keeps the code map current as files change.
    Watch,
    /// Query the code map.
    #[command(subcommand)]
    Query(QueryCmd),
    /// Install a pre-commit hook that runs `gitmind scan`.
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
    Serve,
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
enum QueryCmd {
    /// Print the outline of a single file.
    Outline {
        /// Path relative to the repository root.
        path: String,
        /// Also fetch L2 (docs + calls) — escalates to live extraction if missing.
        #[arg(long)]
        l2: bool,
    },
    /// Search for symbols by name (substring match).
    Symbol {
        needle: String,
        /// Optional kind filter (function, struct, class, etc.).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Find files whose imports mention the given module (heuristic).
    Dependents { module: String },
}

#[derive(Subcommand, Debug)]
enum HookCmd {
    /// Write .git/hooks/pre-commit that invokes `gitmind scan`.
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
    let root = cli
        .root
        .clone()
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));

    match cli.cmd {
        Cmd::Init => cmd_init(&root),
        Cmd::Scan => cmd_scan(&root, verbosity, no_color),
        Cmd::Watch => cmd_watch(&root, verbosity, no_color),
        Cmd::Query(q) => cmd_query(&root, q),
        Cmd::Hook { action } => match action {
            HookCmd::Install => cmd_hook_install(&root),
        },
        Cmd::Lang { action } => match action {
            LangCmd::List => cmd_lang_list(no_color),
            LangCmd::Install => cmd_lang_install(verbosity, no_color),
            LangCmd::Clean => cmd_lang_clean(),
        },
        Cmd::Serve => cmd_serve(&root),
    }
}

fn bootstrap_grammars(verbosity: Verbosity, no_color: bool) -> Result<()> {
    let summary = gitmind::lang::ensure_grammars()
        .map_err(|e| anyhow::anyhow!("grammar bootstrap failed: {e}"))?;
    let mut out = render::stdout(no_color);
    render::render_grammar_bootstrap(&mut out, &summary, verbosity);
    Ok(())
}

fn cmd_init(root: &std::path::Path) -> Result<()> {
    let dir = root.join(config::GITMIND_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = config::config_path(root);
    if path.exists() {
        anyhow::bail!("config already exists at {}", path.display());
    }
    let default_toml = r##""$schema" = "v1"

[scan]
include = ["**/*.rs", "**/*.py", "**/*.ts", "**/*.tsx", "**/*.js", "**/*.go"]
exclude = ["**/target/**", "**/node_modules/**", "**/dist/**", "**/.venv/**", "**/.gitmind/**", "**/.git/**"]
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
    match config::load(root) {
        Ok(c) => Ok(c),
        Err(config::ConfigError::NotFound(_)) => {
            tracing::info!("no .gitmind/gitmind.toml; using defaults");
            Ok(config::default_for_root(root))
        }
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

fn cmd_scan(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default(root)?;
    let mut store = Store::open(root).context("open store")?;
    let report = gitmind::scanner::scan(root, &mut store, &config).context("scan")?;
    let mut out = render::stdout(no_color);
    render::render_report(&mut out, &report, verbosity);
    if report.stats.read_failed + report.stats.extract_failed > 0 {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_watch(root: &std::path::Path, verbosity: Verbosity, no_color: bool) -> Result<()> {
    bootstrap_grammars(verbosity, no_color)?;
    let config = Arc::new(load_or_default(root)?);
    let store = Arc::new(Mutex::new(Store::open(root).context("open store")?));

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
        let cb: gitmind::watcher::BatchCallback =
            Box::new(move |batch: WatchBatch<'_>| match batch.kind {
                BatchKind::InitialScan => {
                    render::render_report(&mut stdout, batch.report, verbosity);
                }
                BatchKind::Incremental { paths } => {
                    render::render_batch_header(&mut stdout, paths, verbosity);
                    render::render_lines(&mut stdout, batch.report, verbosity);
                }
            });
        gitmind::watcher::watch(&root_buf, store_w, config_w, shutdown_rx, cb)
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

fn cmd_query(root: &std::path::Path, q: QueryCmd) -> Result<()> {
    // Query reads cached extracts; grammars are not strictly needed, but the L2 escalation
    // path falls back to live extraction. Bootstrap quietly so first-time L2 doesn't stall.
    let _ = gitmind::lang::ensure_grammars();
    let store = Store::open_read_only(root).context("open store (ro)")?;
    match q {
        QueryCmd::Outline { path, l2 } => {
            let outline = gitmind::query::file_outline(&store, &path)?;
            println!("# {} ({})", path, outline.language);
            if outline.had_errors {
                println!(
                    "  ⚠ {} parse error{} — outline is partial",
                    outline.error_count,
                    if outline.error_count == 1 { "" } else { "s" }
                );
            }
            for s in &outline.symbols {
                let sig = s.signature.as_deref().unwrap_or("");
                println!(
                    "{:>5}:{:<3} {:<10} {:<24} {}",
                    s.start_row + 1,
                    s.start_col,
                    format!("{:?}", s.kind).to_lowercase(),
                    s.name,
                    sig
                );
            }
            if !outline.imports.is_empty() {
                println!("\n## imports");
                for i in &outline.imports {
                    let m = i.module.as_deref().unwrap_or("-");
                    println!("  {m}\t{}", i.raw.replace('\n', " "));
                }
            }
            if l2 {
                let l2 = gitmind::query::file_outline_l2(&store, &path, root)?;
                println!("\n## calls ({})", l2.calls.len());
                for c in &l2.calls {
                    println!("  {}", c.callee);
                }
                println!("\n## docs ({})", l2.docs.len());
            }
            Ok(())
        }
        QueryCmd::Symbol { needle, kind } => {
            let k = kind.as_deref().map(parse_kind).transpose()?;
            let hits = gitmind::query::search_symbols(&store, &needle, k)?;
            for h in hits {
                println!(
                    "{}:{}:{} {} {:?}",
                    h.path,
                    h.symbol.start_row + 1,
                    h.symbol.start_col,
                    h.symbol.name,
                    h.symbol.kind,
                );
            }
            Ok(())
        }
        QueryCmd::Dependents { module } => {
            let hits = gitmind::query::dependents_of(&store, &module)?;
            for h in hits {
                println!("{h}");
            }
            Ok(())
        }
    }
}

fn parse_kind(s: &str) -> Result<SymbolKind> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        other => anyhow::bail!("unknown symbol kind: {other}"),
    })
}

fn cmd_serve(root: &std::path::Path) -> Result<()> {
    // Open the store in read-only mode so we don't conflict with a concurrent `gitmind watch`.
    // The MCP server is purely a query surface.
    let store = Store::open_read_only(root).context("open store (ro)")?;
    let root_buf = root.to_path_buf();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        use rmcp::ServiceExt;
        let server = gitmind::mcp::GitmindServer::new(store, root_buf);
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
    let installed = gitmind::lang::downloaded_languages();
    let supported: std::collections::HashSet<&str> =
        gitmind::lang::SUPPORTED_LANGUAGES.iter().copied().collect();
    let installed_set: std::collections::HashSet<&str> =
        installed.iter().map(String::as_str).collect();

    let ok = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let warn = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    let dim = Style::new().dimmed();

    let _ = writeln!(out, "supported by gitmind (queries shipped):");
    for &name in gitmind::lang::SUPPORTED_LANGUAGES {
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
            "{d}also cached (no gitmind queries, parse-only):{r}",
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

    if let Some(dir) = gitmind::lang::grammar_cache_dir() {
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
        let summary = gitmind::lang::ensure_grammars().map_err(|e| anyhow::anyhow!("{e}"))?;
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
    gitmind::lang::clean_grammar_cache().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("grammar cache cleared");
    Ok(())
}

fn cmd_hook_install(root: &std::path::Path) -> Result<()> {
    let hooks_dir = root.join(".git").join("hooks");
    if !hooks_dir.exists() {
        anyhow::bail!("no .git/hooks directory at {}", hooks_dir.display());
    }
    let hook_path = hooks_dir.join("pre-commit");
    let body = r#"#!/usr/bin/env sh
# Installed by gitmind hook install.
set -e
exec gitmind scan
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
