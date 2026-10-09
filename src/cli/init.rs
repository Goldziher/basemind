//! `basemind init` — the re-runnable onboarding flow.
//!
//! Two side effects, each idempotent and safe to re-run:
//! 1. Write the commented `basemind.toml` scaffold at the repo root (kept, never clobbered, when
//!    one already exists).
//! 2. Inject a "prefer basemind over grep / read / git" rules block into the host repo's
//!    agent-instructions file — an idempotent delimited block in CLAUDE.md / AGENTS.md, or an
//!    ai-rulez rule file when `.ai-rulez/config.toml` owns governance.
//!
//! The index itself is never written into the repo: it lives in a machine-global cache under
//! `~/.local/share/basemind/` (override `BASEMIND_DATA_HOME`), keyed by workspace and served by a
//! background daemon, so there is nothing to gitignore.
//!
//! Capability selection (interactive in a TTY, flag-driven otherwise) narrows which routing rows
//! the rules block advertises. `main.rs` is a thin dispatcher into [`run`]; the block content
//! lives in [`super::init_rules`].

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};

use super::init_gitignore;
use super::init_rules::{self, BlockSections, Capability};
use super::init_settings;
use crate::config;

/// BEGIN delimiter of the managed rules block. Load-bearing: the splice matches this verbatim.
pub(crate) const BEGIN_MARKER: &str = "<!-- BEGIN basemind (managed by `basemind init`) -->";
/// END delimiter of the managed rules block.
pub(crate) const END_MARKER: &str = "<!-- END basemind -->";

/// Fully-commented `basemind.toml` scaffold. Doubles as living documentation: every value shown is
/// the built-in default, so an unedited file is a no-op. Written to the repo ROOT (committed); the
/// index it drives lives in the machine-global cache, so nothing is written into the repo.
pub(crate) const INIT_SCAFFOLD_TOML: &str = r##"# basemind configuration — https://github.com/Goldziher/basemind
# Lives at the repo root, or under `.config/` (`.config/basemind.toml` / `.config/basemind/
# config.toml`), and is meant to be committed. The index (blobs + Fjall) is derived state
# kept in the machine-global cache under ~/.local/share/basemind/ (override BASEMIND_DATA_HOME),
# keyed by workspace and wiped on schema bumps — nothing is written into the repo, so there is
# nothing to gitignore. Never put durable config in the cache.
# Every value below is the built-in default; uncomment and edit only what you want to change.
"$schema" = "v1"

[scan]
# Files to index. Default is "everything"; the tree-sitter language registry + a binary/size check
# filter the long tail. Narrow it if you only care about specific languages. Must not be empty.
# Globs are repo-relative, forward-slash, case-sensitive; `*` also crosses `/`. A bare name with no
# glob characters (`src`, `docs/api`) matches that path and everything beneath it, like .gitignore.
# include = ["**/*"]
# Extra exclude globs, ADDED ON TOP of the always-on floor (node_modules, target, dist, build, out,
# vendor, coverage, venv, .venv, __pycache__, .git, .basemind, bazel-*, .idea, .DS_Store, …). Same
# syntax as include: `generated` excludes every directory of that name; exclude beats include.
# The floor also drops credentials and key material so no secret becomes searchable: .env and
# .env.* (which includes .env.example), .aws, .ssh, .gnupg, .npmrc, .pypirc, .netrc,
# .git-credentials, id_rsa / id_dsa / id_ecdsa / id_ed25519, *.pem, *.key, *.p12, *.pfx, *.jks,
# *.keystore.
# exclude = []
# Floor entries to drop so that tree gets indexed, by directory name, file name or glob (`build`,
# `.env.*`, `*.pem`), or the floor pattern itself. `.git` and `.basemind` can never be allowed, and
# a credential entry is honoured only with the operator's BASEMIND_ALLOW_REPO_CREDENTIALS grant. The
# default exclude also lists dist/target/node_modules/.venv, so remove those from exclude as well
# when allowing them.
# floor_allow = []
# Honor .gitignore / .git/info/exclude while walking. Leave on unless you deliberately want
# ignored files indexed.
# respect_gitignore = true
# Follow symlinks during the walk. Off by default — symlinks often escape the repo (e.g. Bazel's
# bazel-* convenience symlinks). Turn on for repos that symlink real source into place. Ignored in
# this file unless the operator sets BASEMIND_ALLOW_FOLLOW_SYMLINKS=1 in the environment.
# follow_symlinks = false
# Skip files larger than this many bytes (prevents minified-bundle stalls).
# max_file_bytes = 2097152
# Skip paths under any submodule root listed in .gitmodules.
# skip_submodules = true
# Run L2 extraction (calls + docs) inline with L1. Powers the `code` tool's references / callers
# modes. Turning off roughly halves scan time on large repos but leaves reference search empty
# until an L2 pass.
# eager_l2 = true
# Absolute paths OUTSIDE the repo to also index (e.g. a Bazel external cache). Ignored unless the
# operator sets BASEMIND_ALLOW_EXTRA_ROOTS in the environment (1 = any workspace, or a `:`-separated
# list of absolute workspace roots), because this file is authored by the repository. Credential
# directories (.ssh, .aws, .gnupg, /etc) are refused even then. Symlinks inside them are followed
# only when scan.follow_symlinks is on.
# extra_roots = []

# Per-grammar overrides, keyed by tree-sitter-language-pack name (`basemind lang list`).
# enabled = false: stop parsing that grammar's files as code; they fall through to the document
#   tier like any unrecognised file (use [documents] exclude to drop them entirely). Fixes
#   misdetections such as `.txt` (vimdoc) and `.conf` (nginx).
# extensions / filenames: map extra paths onto the grammar (override built-in detection).
# preload = true: `basemind lang install` also fetches this grammar.
# [languages.vimdoc]
# enabled = false
# [languages.jinja2]
# extensions = [".mako", ".tpl"]
# filenames = ["BUILD.in"]
# preload = true

[code_intel]
# Precise, scope- and import-aware name resolution. On by default: JS/TS resolve via oxc, Python and
# Java via the stack-graphs engine, so the `code` tool's references / callers / definition modes
# distinguish a shadowed local from an import instead of matching by name. Set false to fall back to fast
# tree-sitter locals binding for every language. Applies to files (re)scanned after the change.
# precise_resolution = true

[watch]
# Coalesce filesystem events within this window (milliseconds).
# debounce_ms = 250
# Reserved: parsed but has no effect yet.
# live_l2 = false

[cache]
# Reserved: parsed but has no effect yet (see [resources] max_map_cache_mb for the outline cache).
# file_map_lru = 256

[mcp]
# Reserved: "stdio" is the only transport and nothing reads this key.
# transport = "stdio"

[documents]
# Document RAG tier (PDF / Office / HTML / email / images). Requires a `documents` build.
# enabled = true
# Embed documents for semantic search. ON by default — embeddings pay off on real prose / OCR.
# embed = true
# Embedding model preset. Changing it forces a FULL RE-EMBED of the corpus (time + CPU): every
# document is re-encoded at the new model's dimension.
#   fast        — smallest / fastest, lowest quality
#   balanced    — default; 768-dim, good quality/cost tradeoff
#   quality     — larger model, best English quality, slower
#   multilingual— multilingual model for non-English corpora
# embedding_preset = "balanced"
# Globs scoping which non-code files are indexed as documents. Empty include = everything that
# passes [scan]; exclude applies to document indexing itself and beats include.
# include = []
# exclude = []
# Per-document size cap in bytes (separate from scan.max_file_bytes, so big PDFs/Office files work).
# max_file_bytes = 52428800
# Extensions to skip on top of the built-in archive/binary floor (case-insensitive, dot optional).
# extension_denylist = []
# Embedding scope: when embed_include is non-empty only matching documents are embedded; embed_exclude
# always wins. Non-embedded documents stay extracted + keyword-searchable. Changing either (or embed)
# removes the vectors of newly ineligible documents on the next scan.
# embed_include = []
# embed_exclude = []
# Route archives (.zip/.tar/.jar/…) into the recursive archive extractor. Off by default so one
# archive can't explode into thousands of embeds. True binaries are always skipped. In the shared
# daemon this also needs BASEMIND_DAEMON_ALLOW_EXTRACT_ARCHIVES=1 in the daemon's environment.
# extract_archives = false

[code_search]
# Semantic code-search tier. Requires a `code-search` build.
# enabled = true
# Embed source code for VECTOR search. OFF by default — local embeddings on code aren't worth the
# cost (code is embedded with a general English model, and NL→symbol is already served by the BM25
# keyword lane over the same text). Chunking + BM25 keyword search work regardless. Turn on only if
# you specifically want vector search over code (downloads an ONNX model, re-embeds on preset change).
# embed = false
# Embedding scope for source files (only used when embed = true): embed_include is an allow-list
# (empty = all chunked files), embed_exclude wins over it; files left out are still chunked +
# BM25-indexed. Changing either removes the vectors of newly ineligible files on the next scan.
# embed_include = []
# embed_exclude = []

[resources]
# Footprint bounds. 0 = auto for the thread / concurrency caps. The shared daemon treats these as
# ceilings: min(value here, daemon cap), with 0 / "auto" / "off" resolving to the cap. Raise a
# daemon cap with BASEMIND_DAEMON_MAX_SCAN_THREADS / _EMBED_THREADS / _EMBED_BATCH /
# _CONCURRENT_DOCUMENTS / _FOOTPRINT_MB / _MAP_CACHE_MB / _CANDIDATES in the daemon's environment
# (the daemon also caps file, document, crawl and debounce settings; see the configuration docs).
# scan_threads = 0
# embed_threads = 0
# max_concurrent_documents = 0
# embed_batch_size = 32
# max_footprint_mb = 0
# max_map_cache_mb = 256

[crawl]
# Web crawl / scrape limits. allow_private_network = true is ignored when it comes from this file
# unless the operator sets BASEMIND_ALLOW_PRIVATE_HOSTS=1 in the environment.
# respect_robots_txt = true
# allow_private_network = false

[llm]
# Optional LLM for abstractive summaries and LLM NER. Inert while `model` is empty. In this file
# `base_url` is ignored and api_key env references are limited to the provider's standard variable
# (OPENAI_API_KEY for openai/..., ANTHROPIC_API_KEY for anthropic/..., or BASEMIND_LLM_API_KEY)
# unless the operator sets BASEMIND_ALLOW_REPO_LLM=1 in the environment.
# model = "openai/gpt-4o"
# api_key = { env = "OPENAI_API_KEY" }
"##;

/// Where to inject the usage rules. `Auto` never writes a COMMITTED agent-instructions file
/// (`CLAUDE.md` / `AGENTS.md`) without an explicit opt-in: it routes to ai-rulez when that owns
/// governance, otherwise to the personal, gitignored `*.local.md` sibling. The interactive prompt
/// and the `/bm-init` slash command offer the committed files as an explicit choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RulesTarget {
    /// Detect the target automatically (default): ai-rulez if it owns governance, else the
    /// gitignored `CLAUDE.local.md` / `AGENTS.local.md` — never a committed file unasked.
    #[default]
    Auto,
    /// Force the committed `CLAUDE.md` delimited block.
    Claude,
    /// Force the personal, gitignored `CLAUDE.local.md` delimited block.
    ClaudeLocal,
    /// Force the committed `AGENTS.md` delimited block.
    Agents,
    /// Force the personal, gitignored `AGENTS.local.md` delimited block.
    AgentsLocal,
    /// Force the COMMITTED ai-rulez rule file (`.ai-rulez/rules/basemind-usage.md`). An explicit
    /// opt-in only — `Auto` never resolves here, it prefers [`RulesTarget::AiRulezLocal`].
    AiRulez,
    /// Force the personal, gitignored ai-rulez rule file (`.ai-rulez/local/rules/basemind-usage.md`).
    /// This is what `Auto` resolves to when `.ai-rulez/config.toml` owns governance.
    AiRulezLocal,
    /// Write no rules at all (same as `--no-rules`).
    None,
}

/// Clap value parser accepting exactly the [`Capability`] slugs; keeps `--help` and validation in
/// sync with the enum.
fn capability_slugs() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(Capability::ALL.map(Capability::slug))
}

/// Flags for `basemind init`. Flattened into the `Cmd::Init` clap variant in `main.rs`.
#[derive(Args, Debug, Default)]
pub struct InitArgs {
    /// Accept defaults non-interactively: enable every capability unless narrowed by
    /// `--with` / `--without`. Implied automatically when stdin is not a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Enable only these capabilities (repeatable); the accepted slugs are listed below.
    #[arg(long = "with", value_name = "CAPABILITY", value_parser = capability_slugs())]
    pub with: Vec<String>,

    /// Disable these capabilities (repeatable). Same slugs as `--with`.
    #[arg(long = "without", value_name = "CAPABILITY", value_parser = capability_slugs())]
    pub without: Vec<String>,

    /// Where to inject usage rules. `auto` (default) detects the source of truth.
    #[arg(long, value_enum, default_value_t = RulesTarget::Auto)]
    pub rules_target: RulesTarget,

    /// Where to add basemind's MCP tools to Claude Code's auto-approved permissions
    /// (`permissions.allow` in `.claude/settings*.json`). Unset: interactive TTY prompts
    /// (recommending `local`); a non-interactive run (`--yes` / piped) skips this step entirely
    /// unless the flag is passed explicitly — broadening auto-approved permissions is not
    /// something to land unattended.
    #[arg(long, value_enum)]
    pub settings_target: Option<init_settings::SettingsTarget>,

    /// Skip rules injection entirely (write the config scaffold only).
    #[arg(long)]
    pub no_rules: bool,

    /// Omit the usage-priority / routing-table section from the block.
    #[arg(long)]
    pub no_usage_rules: bool,

    /// Omit the setup & maintenance section from the block.
    #[arg(long)]
    pub no_setup_notes: bool,

    /// Dry run: print what WOULD change and write nothing.
    #[arg(long)]
    pub print: bool,

    /// Write the config under the project-level `.config/` convention instead of the repo root.
    /// Accepted values: `.config` (writes `.config/basemind.toml`) or `.config/basemind` (writes
    /// `.config/basemind/config.toml`). Any other value is rejected — basemind auto-discovers only
    /// these locations, so a config written elsewhere would never be read.
    #[arg(long, value_name = "DIR")]
    pub config_dir: Option<String>,
}

/// One planned filesystem effect, collected before anything is written so `--print` can report a
/// faithful dry-run and the real run reports the same set. `pub(crate)` so [`init_settings`] can
/// build one for the settings-permissions step.
pub(crate) enum Change {
    /// A file will be created or its content changed. `note` is the human summary.
    Write {
        path: PathBuf,
        note: &'static str,
        contents: String,
    },
    /// Nothing to do for this target (already converged / opted out).
    NoOp { note: String },
}

/// Entry point. `root` is already resolved by `main`.
pub fn run(root: &Path, args: &InitArgs) -> Result<()> {
    let caps = select_capabilities(args)?;
    let sections = BlockSections {
        usage_rules: !args.no_usage_rules,
        setup_notes: !args.no_setup_notes,
    };

    let rules_target = resolve_rules_target(root, args)?;
    let rules_plan = resolve_rules_plan(root, rules_target, args.no_rules);
    let settings_target = init_settings::resolve_settings_target(args)?;
    let settings_plan = init_settings::resolve_settings_plan(root, settings_target);

    let mut changes = Vec::new();
    changes.push(plan_config(root, args.config_dir.as_deref())?);
    if let Some(rule_change) = plan_rules_change(&rules_plan, &caps, sections)? {
        changes.push(rule_change);
    }
    if let Some(settings_change) = init_settings::plan_settings_change(&settings_plan)? {
        changes.push(settings_change);
    }

    let gitignore_targets = gitignore_targets(&rules_plan, &settings_plan);

    if args.print {
        report_dry_run(&changes);
        for target in &gitignore_targets {
            if let Some(pattern) = init_gitignore::local_pattern(root, target)
                && !init_gitignore::is_covered(root, target)?
            {
                println!("would add {pattern:?} to .gitignore ({})", target.display());
            }
        }
        return Ok(());
    }

    // ~keep Gitignore coverage runs before the writes below: it never depends on the target
    // ~keep file existing (a `.gitignore` pattern matches a path whether or not it's on disk yet),
    // ~keep and doing it first means a `.local` file is never written uncovered, even for an instant.
    for target in &gitignore_targets {
        if let Some(pattern) = init_gitignore::local_pattern(root, target)
            && let Some(gitignore_path) = init_gitignore::ensure_coverage(root, target, &pattern, args.yes)?
        {
            println!("added {pattern:?} to .gitignore: {}", gitignore_path.display());
        }
    }

    let mut any_write = false;
    for change in &changes {
        match change {
            Change::Write { path, note, contents } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
                println!("{note}: {}", path.display());
                any_write = true;
            }
            Change::NoOp { note } => println!("{note}"),
        }
    }
    if any_write {
        println!("basemind init: done.");
    } else {
        println!("basemind init: nothing to do — already up to date.");
    }
    Ok(())
}

/// Every path this run's resolved plans would write, that must not land uncommitted-by-convention
/// without `.gitignore` covering it. `Skip` plans and committed (non-`.local`) targets contribute
/// nothing — [`init_gitignore::local_pattern`] filters those out.
fn gitignore_targets(rules_plan: &RulesPlan, settings_plan: &init_settings::SettingsPlan) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    if let Some(path) = rules_plan.target_path() {
        targets.push(path.to_path_buf());
    }
    if let init_settings::SettingsPlan::Write(path) = settings_plan {
        targets.push(path.clone());
    }
    targets
}

/// Decide the effective rules target, applying the "ask before touching a committed file" policy.
///
/// Precedence: an explicit `--rules-target` (anything but `auto`) or `--no-rules` wins verbatim;
/// then ai-rulez governance when `.ai-rulez/config.toml` is present; then — only in an interactive
/// TTY — prompt the user to choose between the personal `*.local.md` files and the committed
/// `CLAUDE.md` / `AGENTS.md`. Non-interactively (`--yes` / piped) `Auto` is returned unchanged and
/// [`resolve_rules_plan`] applies its safe local default, so a scripted run never silently edits a
/// committed agent-instructions file.
fn resolve_rules_target(root: &Path, args: &InitArgs) -> Result<RulesTarget> {
    if args.no_rules {
        return Ok(RulesTarget::None);
    }
    if args.rules_target != RulesTarget::Auto {
        return Ok(args.rules_target);
    }
    if root.join(".ai-rulez").join("config.toml").exists() {
        // ~keep ai-rulez owns governance here, but the rule file basemind writes is its own
        // ~keep tool-usage advice, not something the repo's ai-rulez maintainers authored — treat
        // ~keep it like any other basemind-owned file and default to the gitignored `.local` tree.
        // ~keep `--rules-target ai-rulez` still opts into the committed file explicitly.
        return Ok(RulesTarget::AiRulezLocal);
    }
    if !args.yes && std::io::stdin().is_terminal() {
        return prompt_rules_target();
    }
    Ok(RulesTarget::Auto)
}

/// Interactive prompt for where the rules block should land. Hand-rolled over stdin (no new crate).
/// A blank answer accepts the recommended default: the personal, gitignored `CLAUDE.local.md`, so
/// the committed, shared `CLAUDE.md` / `AGENTS.md` are only written when explicitly chosen.
fn prompt_rules_target() -> Result<RulesTarget> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    println!("Where should the basemind rules block go?");
    println!("  1) CLAUDE.local.md  — personal, gitignored (recommended)");
    println!("  2) CLAUDE.md        — committed, shared with everyone on the repo");
    println!("  3) AGENTS.local.md  — personal, gitignored");
    println!("  4) AGENTS.md        — committed, shared with everyone on the repo");
    println!("  5) none             — write no rules");
    write!(stdout, "Choose [1-5, blank = 1]: ").context("write prompt")?;
    stdout.flush().context("flush prompt")?;
    let mut line = String::new();
    stdin.read_line(&mut line).context("read stdin")?;
    Ok(match line.trim() {
        "" | "1" => RulesTarget::ClaudeLocal,
        "2" => RulesTarget::Claude,
        "3" => RulesTarget::AgentsLocal,
        "4" => RulesTarget::Agents,
        "5" => RulesTarget::None,
        // ~keep An unrecognized answer falls back to the safe, gitignored recommendation rather
        // ~keep than guessing a committed file.
        _ => RulesTarget::ClaudeLocal,
    })
}

/// Resolve the selected capability set from flags + interactivity.
fn select_capabilities(args: &InitArgs) -> Result<Vec<Capability>> {
    let with = parse_capabilities(&args.with)?;
    let without = parse_capabilities(&args.without)?;

    // ~keep Explicit `--with` is an allow-list; otherwise start from "all on".
    let base: Vec<Capability> = if with.is_empty() {
        Capability::ALL.to_vec()
    } else {
        with.clone()
    };

    let interactive = !args.yes && with.is_empty() && without.is_empty() && std::io::stdin().is_terminal();
    let selected: Vec<Capability> = if interactive {
        prompt_capabilities()?
    } else {
        base.into_iter().filter(|c| !without.contains(c)).collect()
    };
    Ok(selected)
}

/// Parse a list of capability slugs into [`Capability`], erroring on an unknown slug.
fn parse_capabilities(slugs: &[String]) -> Result<Vec<Capability>> {
    slugs
        .iter()
        .map(|s| {
            Capability::from_slug(s).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown capability {s:?}; expected one of: {}",
                    Capability::ALL.map(|c| c.slug()).join(", ")
                )
            })
        })
        .collect()
}

/// Interactive yes/no prompt per capability. Hand-rolled over stdin — no new crate dependency.
/// A blank answer accepts the default (yes).
fn prompt_capabilities() -> Result<Vec<Capability>> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    println!("Select basemind capabilities to advertise (Y/n, blank = yes):");
    let mut selected = Vec::new();
    for cap in Capability::ALL {
        write!(stdout, "  {} [Y/n] ", cap.label()).context("write prompt")?;
        stdout.flush().context("flush prompt")?;
        let mut line = String::new();
        let read = stdin.read_line(&mut line).context("read stdin")?;
        // ~keep EOF mid-prompt: accept the default for this and every remaining capability.
        let answer = line.trim().to_ascii_lowercase();
        let yes = answer.is_empty() || answer == "y" || answer == "yes";
        if yes {
            selected.push(cap);
        }
        if read == 0 {
            break;
        }
    }
    Ok(selected)
}

/// Plan the `basemind.toml` write: scaffold when absent, keep when present.
fn plan_config(root: &Path, config_dir: Option<&str>) -> Result<Change> {
    let path = resolve_init_config_path(root, config_dir)?;
    if path.exists() {
        return Ok(Change::NoOp {
            note: format!("basemind.toml: kept existing config at {}", path.display()),
        });
    }
    // ~keep A ROOT scaffold would silently shadow the legacy config (root path wins in resolution), so
    // ~keep refuse rather than change the effective config behind the user's back. A `.config/` scaffold
    // ~keep does NOT shadow it — resolution reads legacy before the convention — so only the root target
    // ~keep is guarded. They migrate it, then re-run init. Matches the pre-onboarding `cmd_init`
    // ~keep contract (config_root_smoke).
    let legacy = config::legacy_config_path(root);
    if path == config::config_path(root) && legacy.exists() {
        anyhow::bail!(
            "legacy config at {} is still read as a fallback; move it to {} to migrate — init will \
             not write a scaffold that shadows it",
            legacy.display(),
            path.display()
        );
    }
    Ok(Change::Write {
        path,
        note: "wrote basemind.toml scaffold",
        contents: INIT_SCAFFOLD_TOML.to_string(),
    })
}

/// Map `--config-dir` to the config file `init` writes. Only the two `.config/` convention spellings
/// are accepted: there is no read-time config-path flag, so a file written anywhere else would never
/// be auto-discovered — silently producing a config basemind ignores. Unset writes the canonical
/// root `basemind.toml`.
fn resolve_init_config_path(root: &Path, config_dir: Option<&str>) -> Result<PathBuf> {
    let Some(dir) = config_dir.map(str::trim).filter(|dir| !dir.is_empty()) else {
        return Ok(config::config_path(root));
    };
    let normalized = dir.trim_start_matches("./").trim_end_matches('/');
    if normalized == config::CONFIG_CONVENTION_DIR {
        return Ok(config::convention_flat_config_path(root));
    }
    if normalized == format!("{}/{}", config::CONFIG_CONVENTION_DIR, config::CONFIG_CONVENTION_SUBDIR) {
        return Ok(config::convention_nested_config_path(root));
    }
    anyhow::bail!(
        "unsupported --config-dir {dir:?}: basemind auto-discovers only the repo root, \
         `.config/basemind.toml`, and `.config/basemind/config.toml` — pass `.config` or \
         `.config/basemind`"
    )
}

/// Which agent-instructions file owns the rules, after resolving `--rules-target`/`--no-rules`
/// against the detection priority.
#[derive(Debug)]
enum RulesPlan {
    /// Skip rules entirely.
    Skip,
    /// Write an ai-rulez rule file at this path.
    AiRulez(PathBuf),
    /// Splice the delimited block into this markdown file (creating it if absent).
    Delimited(PathBuf),
}

/// Resolve where the rules go from an already-decided `target` (the interactive prompt / ai-rulez
/// precedence is applied upstream in [`resolve_rules_target`]). `Auto` here is the NON-interactive
/// fallback and is deliberately safe: it never writes a committed `CLAUDE.md` / `AGENTS.md`, only
/// the gitignored `*.local.md` sibling (or ai-rulez when it owns governance).
fn resolve_rules_plan(root: &Path, target: RulesTarget, no_rules: bool) -> RulesPlan {
    if no_rules || target == RulesTarget::None {
        return RulesPlan::Skip;
    }
    let ai_rulez_rule = root.join(".ai-rulez").join("rules").join("basemind-usage.md");
    let ai_rulez_local_rule = root
        .join(".ai-rulez")
        .join("local")
        .join("rules")
        .join("basemind-usage.md");
    let claude = root.join("CLAUDE.md");
    let claude_local = root.join("CLAUDE.local.md");
    let agents = root.join("AGENTS.md");
    let agents_local = root.join("AGENTS.local.md");
    match target {
        RulesTarget::AiRulez => RulesPlan::AiRulez(ai_rulez_rule),
        RulesTarget::AiRulezLocal => RulesPlan::AiRulez(ai_rulez_local_rule),
        RulesTarget::Claude => RulesPlan::Delimited(claude),
        RulesTarget::ClaudeLocal => RulesPlan::Delimited(claude_local),
        RulesTarget::Agents => RulesPlan::Delimited(agents),
        RulesTarget::AgentsLocal => RulesPlan::Delimited(agents_local),
        RulesTarget::None => RulesPlan::Skip,
        RulesTarget::Auto => {
            // ~keep Never auto-write a COMMITTED agent-instructions file — that's a shared,
            // ~keep version-controlled surface the user must opt into. Prefer the gitignored
            // ~keep ai-rulez `.local` rule tree when ai-rulez owns governance, else the personal
            // ~keep `*.local.md` sibling, matching whichever committed convention the repo already
            // ~keep uses (AGENTS-only repos get AGENTS.local.md).
            if root.join(".ai-rulez").join("config.toml").exists() {
                RulesPlan::AiRulez(ai_rulez_local_rule)
            } else if agents.exists() && !claude.exists() {
                RulesPlan::Delimited(agents_local)
            } else {
                RulesPlan::Delimited(claude_local)
            }
        }
    }
}

impl RulesPlan {
    /// The target path this plan would write, or `None` for [`RulesPlan::Skip`]. Used by the
    /// `.gitignore`-coverage step, which needs the path regardless of whether the content is
    /// actually changing this run.
    fn target_path(&self) -> Option<&Path> {
        match self {
            RulesPlan::Skip => None,
            RulesPlan::AiRulez(path) | RulesPlan::Delimited(path) => Some(path),
        }
    }
}

/// Plan the rules write from an already-resolved `plan`. Returns `None` only when rules are
/// skipped.
fn plan_rules_change(plan: &RulesPlan, caps: &[Capability], sections: BlockSections) -> Result<Option<Change>> {
    match plan {
        RulesPlan::Skip => Ok(Some(Change::NoOp {
            note: "rules: skipped (--no-rules / --rules-target none)".to_string(),
        })),
        RulesPlan::AiRulez(path) => {
            let contents = init_rules::render_ai_rulez_rule(caps, sections);
            let unchanged = std::fs::read_to_string(path).is_ok_and(|prev| prev == contents);
            if unchanged {
                return Ok(Some(Change::NoOp {
                    note: format!("rules: ai-rulez rule already up to date ({})", path.display()),
                }));
            }
            Ok(Some(Change::Write {
                path: path.clone(),
                note: "wrote ai-rulez rule (run `ai-rulez generate` to render outputs)",
                contents,
            }))
        }
        RulesPlan::Delimited(path) => {
            let block = format!(
                "{BEGIN_MARKER}\n\n{}{END_MARKER}\n",
                init_rules::render_block_body(caps, sections)
            );
            let existing = match std::fs::read_to_string(path) {
                Ok(c) => Some(c),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
            };
            let next = splice_block(existing.as_deref(), &block)
                .with_context(|| format!("update basemind rules block in {}", path.display()))?;
            if existing.as_deref() == Some(next.as_str()) {
                return Ok(Some(Change::NoOp {
                    note: format!("rules: block already up to date ({})", path.display()),
                }));
            }
            Ok(Some(Change::Write {
                path: path.clone(),
                note: "injected basemind rules block",
                contents: next,
            }))
        }
    }
}

/// Splice the managed `block` into `existing` markdown idempotently: replace between the markers if
/// present, else append at EOF. Content outside the markers is preserved verbatim.
///
/// Bails (rather than guessing) when the markers are malformed — one marker present without its pair,
/// END before BEGIN, or a second BEGIN inside the block — because every such state makes the block
/// bounds ambiguous, and guessing risks silently deleting the user's own content between a stray
/// marker and the wrong pair. A hard stop asking the user to fix the markers is always safer.
fn splice_block(existing: Option<&str>, block: &str) -> Result<String> {
    let Some(existing) = existing else {
        return Ok(block.to_string());
    };
    match (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        (Some(begin), Some(end_start)) => {
            let begin_body = begin + BEGIN_MARKER.len();
            if end_start < begin_body {
                anyhow::bail!(
                    "malformed basemind block: END marker precedes BEGIN marker — resolve the markers manually then re-run"
                );
            }
            if existing[begin_body..end_start].contains(BEGIN_MARKER) {
                anyhow::bail!(
                    "malformed basemind block: a second BEGIN marker before the END marker — resolve the markers manually then re-run"
                );
            }
            let end = end_start + END_MARKER.len();
            // ~keep Absorb a single trailing newline (LF or CRLF) after the END marker so replacement is byte-stable.
            let after = &existing[end..];
            let tail_start = if after.starts_with("\r\n") {
                end + 2
            } else if after.starts_with('\n') {
                end + 1
            } else {
                end
            };
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..begin]);
            out.push_str(block);
            out.push_str(&existing[tail_start..]);
            Ok(out)
        }
        (Some(_), None) | (None, Some(_)) => anyhow::bail!(
            "malformed basemind block: only one of the BEGIN/END markers is present — resolve the markers manually then re-run"
        ),
        (None, None) => {
            // ~keep No markers: append at EOF with a blank-line separator.
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            Ok(out)
        }
    }
}

/// Print a faithful dry-run of the planned changes and note that nothing was written.
fn report_dry_run(changes: &[Change]) {
    let mut pending = 0;
    for change in changes {
        match change {
            Change::Write { path, note, .. } => {
                println!("would {note}: {}", path.display());
                pending += 1;
            }
            Change::NoOp { note } => println!("{note}"),
        }
    }
    if pending == 0 {
        println!("basemind init --print: no changes — already up to date.");
    } else {
        println!("basemind init --print: {pending} file(s) would change (nothing written).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: InitArgs,
    }

    /// `--with` / `--without` must accept and advertise every `Capability`, so the help cannot drift.
    #[test]
    fn capability_flags_cover_every_capability_slug() {
        let mut cmd = Wrapper::command();
        let help = cmd.render_long_help().to_string();
        for cap in Capability::ALL {
            let slug = cap.slug();
            assert!(help.contains(slug), "help omits `{slug}`");
            let parsed = Wrapper::try_parse_from(["x", "--with", slug, "--without", slug]).expect("slug accepted");
            assert_eq!(parsed.args.with, [slug]);
        }
        assert!(Wrapper::try_parse_from(["x", "--with", "bogus"]).is_err());
    }

    /// Uncommenting every `# key = value` line of the scaffold must yield a config that parses and
    /// validates, so a documented key that drifts from the schema fails here.
    #[test]
    fn scaffold_examples_parse_when_uncommented() {
        let mut uncommented = String::new();
        for line in INIT_SCAFFOLD_TOML.lines() {
            let candidate = line.strip_prefix("# ").unwrap_or(line);
            let is_example = line.starts_with("# ")
                && !candidate.starts_with(' ')
                && (candidate.starts_with('[')
                    || toml::from_str::<toml::Table>(candidate).is_ok_and(|t| !t.is_empty()));
            uncommented.push_str(if is_example { candidate } else { line });
            uncommented.push('\n');
        }
        let cfg = config::parse_str(&uncommented).expect("uncommented scaffold parses and validates");
        assert_eq!(
            cfg.languages.len(),
            2,
            "language examples are live: {:?}",
            cfg.languages
        );
        assert!(cfg.languages["jinja2"].preload);
        assert!(!cfg.languages["vimdoc"].enabled);
        assert_eq!(cfg.documents.max_file_bytes, 52_428_800);
        assert!(cfg.scan.floor_allow.is_empty());
    }

    #[test]
    fn splice_appends_block_when_no_markers() {
        let out = splice_block(
            Some("# Title\n\nbody\n"),
            "<!-- BEGIN basemind (managed by `basemind init`) -->\nX\n<!-- END basemind -->\n",
        )
        .expect("well-formed input splices");
        assert!(out.starts_with("# Title\n\nbody\n"), "user content preserved");
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn splice_replaces_in_place_and_is_idempotent() {
        let block = format!("{BEGIN_MARKER}\nv2\n{END_MARKER}\n");
        let before = format!("intro\n\n{BEGIN_MARKER}\nv1\n{END_MARKER}\n\noutro\n");
        let after = splice_block(Some(&before), &block).expect("well-formed input splices");
        assert_eq!(after.matches(BEGIN_MARKER).count(), 1, "no duplicate block");
        assert!(after.contains("v2") && !after.contains("v1"), "content replaced");
        assert!(
            after.contains("intro") && after.contains("outro"),
            "surrounding content kept"
        );
        // ~keep Re-splicing the same block is a fixpoint.
        assert_eq!(splice_block(Some(&after), &block).expect("fixpoint"), after);
    }

    #[test]
    fn splice_bails_on_orphaned_begin_marker() {
        // ~keep A lone BEGIN (END deleted by hand) must NOT append a second block — that would
        // ~keep leave a two-BEGIN/one-END state a later run could collapse, eating user content.
        let block = format!("{BEGIN_MARKER}\nv2\n{END_MARKER}\n");
        let orphaned = format!("intro\n{BEGIN_MARKER}\nv1\nno end here\noutro\n");
        assert!(
            splice_block(Some(&orphaned), &block).is_err(),
            "orphaned BEGIN must bail"
        );
    }

    #[test]
    fn splice_bails_on_reversed_and_doubled_markers() {
        let block = format!("{BEGIN_MARKER}\nv2\n{END_MARKER}\n");
        // ~keep END before BEGIN — bounds are inverted, refuse to guess.
        let reversed = format!("{END_MARKER}\nstray\n{BEGIN_MARKER}\n");
        assert!(
            splice_block(Some(&reversed), &block).is_err(),
            "reversed markers must bail"
        );
        // ~keep Two BEGINs before the END — ambiguous which block to replace.
        let doubled = format!("{BEGIN_MARKER}\na\n{BEGIN_MARKER}\nb\n{END_MARKER}\n");
        assert!(splice_block(Some(&doubled), &block).is_err(), "doubled BEGIN must bail");
    }

    #[test]
    fn splice_converges_to_fixpoint_on_crlf_file() {
        // ~keep A CRLF-authored rules file must reach a byte-stable fixpoint so `--print` stops
        // ~keep reporting a pending change and re-runs are no-ops (idempotency contract).
        let block = format!("{BEGIN_MARKER}\nv2\n{END_MARKER}\n");
        let crlf = format!("intro\r\n{BEGIN_MARKER}\r\nv1\r\n{END_MARKER}\r\noutro\r\n");
        let once = splice_block(Some(&crlf), &block).expect("first splice");
        let twice = splice_block(Some(&once), &block).expect("second splice");
        assert_eq!(once, twice, "CRLF file must converge to a fixpoint");
        assert_eq!(once.matches(BEGIN_MARKER).count(), 1, "single block");
        assert!(once.contains("outro"), "trailing user content kept");
    }

    #[test]
    fn none_target_skips_rules() {
        assert!(matches!(
            resolve_rules_plan(Path::new("/nonexistent"), RulesTarget::None, false),
            RulesPlan::Skip
        ));
    }

    #[test]
    fn auto_never_targets_a_committed_file() {
        // ~keep The non-interactive Auto fallback must resolve to a gitignored *.local.md sibling,
        // ~keep never the committed CLAUDE.md / AGENTS.md.
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve_rules_plan(dir.path(), RulesTarget::Auto, false) {
            RulesPlan::Delimited(path) => assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some("CLAUDE.local.md"),
                "fresh repo Auto must land in CLAUDE.local.md, got {}",
                path.display()
            ),
            other => panic!("expected a delimited CLAUDE.local.md plan, got {other:?}"),
        }

        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").expect("seed AGENTS.md");
        match resolve_rules_plan(dir.path(), RulesTarget::Auto, false) {
            RulesPlan::Delimited(path) => assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some("AGENTS.local.md"),
                "an AGENTS.md-only repo must land in AGENTS.local.md, got {}",
                path.display()
            ),
            other => panic!("expected a delimited AGENTS.local.md plan, got {other:?}"),
        }
    }

    #[test]
    fn explicit_local_and_committed_targets_resolve_to_their_files() {
        let root = Path::new("/nonexistent");
        let name = |plan: RulesPlan| match plan {
            RulesPlan::Delimited(path) => path.file_name().and_then(|n| n.to_str()).map(str::to_owned),
            other => panic!("expected delimited plan, got {other:?}"),
        };
        assert_eq!(
            name(resolve_rules_plan(root, RulesTarget::ClaudeLocal, false)).as_deref(),
            Some("CLAUDE.local.md")
        );
        assert_eq!(
            name(resolve_rules_plan(root, RulesTarget::Claude, false)).as_deref(),
            Some("CLAUDE.md")
        );
        assert_eq!(
            name(resolve_rules_plan(root, RulesTarget::AgentsLocal, false)).as_deref(),
            Some("AGENTS.local.md")
        );
        assert_eq!(
            name(resolve_rules_plan(root, RulesTarget::Agents, false)).as_deref(),
            Some("AGENTS.md")
        );
    }

    #[test]
    fn auto_resolves_to_ai_rulez_local_when_ai_rulez_owns_governance() {
        // ~keep When `.ai-rulez/config.toml` is present, Auto must route to the gitignored
        // ~keep `.ai-rulez/local/` tree, never the committed `.ai-rulez/rules/` one — the whole
        // ~keep point of `AiRulezLocal` is that Auto never writes a committed file unasked.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".ai-rulez")).expect("mkdir .ai-rulez");
        std::fs::write(dir.path().join(".ai-rulez").join("config.toml"), "").expect("seed config.toml");

        match resolve_rules_plan(dir.path(), RulesTarget::Auto, false) {
            RulesPlan::AiRulez(path) => assert_eq!(
                path,
                dir.path()
                    .join(".ai-rulez")
                    .join("local")
                    .join("rules")
                    .join("basemind-usage.md"),
                "Auto with ai-rulez present must resolve to the local rule tree"
            ),
            other => panic!("expected an AiRulez plan, got {other:?}"),
        }

        assert_eq!(
            resolve_rules_target(dir.path(), &InitArgs::default()).expect("resolve target"),
            RulesTarget::AiRulezLocal,
            "resolve_rules_target must also route to AiRulezLocal, not the committed AiRulez"
        );
    }

    #[test]
    fn explicit_ai_rulez_and_ai_rulez_local_resolve_to_their_own_files() {
        let root = Path::new("/nonexistent");
        match resolve_rules_plan(root, RulesTarget::AiRulez, false) {
            RulesPlan::AiRulez(path) => {
                assert_eq!(path, root.join(".ai-rulez").join("rules").join("basemind-usage.md"))
            }
            other => panic!("expected an AiRulez plan, got {other:?}"),
        }
        match resolve_rules_plan(root, RulesTarget::AiRulezLocal, false) {
            RulesPlan::AiRulez(path) => assert_eq!(
                path,
                root.join(".ai-rulez")
                    .join("local")
                    .join("rules")
                    .join("basemind-usage.md")
            ),
            other => panic!("expected an AiRulez plan, got {other:?}"),
        }
    }
}
