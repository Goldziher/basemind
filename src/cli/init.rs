//! `basemind init` — the re-runnable onboarding flow.
//!
//! Three side effects, each idempotent and safe to re-run:
//! 1. Write the commented `basemind.toml` scaffold at the repo root (kept, never clobbered, when
//!    one already exists).
//! 2. Ensure `.basemind/` is gitignored.
//! 3. Inject a "prefer basemind over grep / read / git" rules block into the host repo's
//!    agent-instructions file — an idempotent delimited block in CLAUDE.md / AGENTS.md, or an
//!    ai-rulez rule file when `.ai-rulez/config.toml` owns governance.
//!
//! Capability selection (interactive in a TTY, flag-driven otherwise) narrows which routing rows
//! the rules block advertises. `main.rs` is a thin dispatcher into [`run`]; the block content
//! lives in [`super::init_rules`].

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};

use super::init_rules::{self, BlockSections, Capability};
use crate::config;

/// BEGIN delimiter of the managed rules block. Load-bearing: the splice matches this verbatim.
pub(crate) const BEGIN_MARKER: &str = "<!-- BEGIN basemind (managed by `basemind init`) -->";
/// END delimiter of the managed rules block.
pub(crate) const END_MARKER: &str = "<!-- END basemind -->";

/// Fully-commented `basemind.toml` scaffold. Doubles as living documentation: every value shown is
/// the built-in default, so an unedited file is a no-op. Written to the repo ROOT (committed);
/// the `.basemind/` cache it drives is gitignored.
pub(crate) const INIT_SCAFFOLD_TOML: &str = r##"# basemind configuration — https://github.com/Goldziher/basemind
# Lives at the repo root and is meant to be committed. The `.basemind/` cache (index + blobs) is
# derived state, gitignored, and wiped on schema bumps — never put durable config there.
# Every value below is the built-in default; uncomment and edit only what you want to change.
"$schema" = "v1"

[scan]
# Files to index. Default is "everything"; the tree-sitter language registry + a binary/size check
# filter the long tail. Narrow it if you only care about specific languages.
# include = ["**/*"]
# Extra exclude globs, ADDED ON TOP of the always-on floor (node_modules, target, dist, .venv,
# __pycache__, .git, .basemind, bazel-*, .idea, .DS_Store, …). You cannot remove a floor entry.
# exclude = []
# Honor .gitignore / .git/info/exclude while walking. Leave on unless you deliberately want
# ignored files indexed.
# respect_gitignore = true
# Follow symlinks during the walk. Off by default — symlinks often escape the repo (e.g. Bazel's
# bazel-* convenience symlinks). Turn on for repos that symlink real source into place.
# follow_symlinks = false
# Skip files larger than this many bytes (prevents minified-bundle stalls).
# max_file_bytes = 2097152
# Skip paths under any submodule root listed in .gitmodules.
# skip_submodules = true
# Run L2 extraction (calls + docs) inline with L1. Powers find_references / find_callers. Turning
# off roughly halves scan time on large repos but leaves reference search empty until an L2 pass.
# eager_l2 = true
# Absolute paths OUTSIDE the repo to also index (e.g. a Bazel external cache). Symlinks are always
# followed for these regardless of scan.follow_symlinks.
# extra_roots = []

[watch]
# Coalesce filesystem events within this window (milliseconds).
# debounce_ms = 250
# Run L2 extraction on live watch edits (extra CPU per edit).
# live_l2 = false

[cache]
# Max extracted file-maps kept hot in memory.
# file_map_lru = 256

[mcp]
# MCP transport. Only "stdio" is supported today.
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
# Globs for documents that are still extracted + indexed but NOT embedded (keyword-only).
# embed_exclude = []
# Route archives (.zip/.tar/.jar/…) into the recursive archive extractor. Off by default so one
# archive can't explode into thousands of embeds. True binaries are always skipped.
# extract_archives = false

[code_search]
# Semantic code-search tier. Requires a `code-search` build.
# enabled = true
# Embed source code for VECTOR search. OFF by default — local embeddings on code aren't worth the
# cost (code is embedded with a general English model, and NL→symbol is already served by the BM25
# keyword lane over the same text). Chunking + BM25 keyword search work regardless. Turn on only if
# you specifically want vector search over code (downloads an ONNX model, re-embeds on preset change).
# embed = false
# Globs for source files that are still chunked + BM25-indexed but NOT embedded (only used when
# embed = true).
# embed_exclude = []
"##;

/// Where to inject the usage rules. `Auto` follows detection priority
/// (ai-rulez → CLAUDE.md → AGENTS.md → create CLAUDE.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RulesTarget {
    /// Detect the source of truth automatically (default).
    #[default]
    Auto,
    /// Force the CLAUDE.md delimited block.
    Claude,
    /// Force the AGENTS.md delimited block.
    Agents,
    /// Force an ai-rulez rule file (`.ai-rulez/rules/basemind-usage.md`).
    AiRulez,
    /// Write no rules at all (same as `--no-rules`).
    None,
}

/// Flags for `basemind init`. Flattened into the `Cmd::Init` clap variant in `main.rs`.
#[derive(Args, Debug, Default)]
pub struct InitArgs {
    /// Accept defaults non-interactively: enable every capability unless narrowed by
    /// `--with` / `--without`. Implied automatically when stdin is not a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Enable only these capabilities (repeatable). Slugs: code-search-navigation,
    /// code-mapping-architecture, git-history, agent-comms, documents-rag, semantic-search.
    #[arg(long = "with", value_name = "CAPABILITY")]
    pub with: Vec<String>,

    /// Disable these capabilities (repeatable). Same slugs as `--with`.
    #[arg(long = "without", value_name = "CAPABILITY")]
    pub without: Vec<String>,

    /// Where to inject usage rules. `auto` (default) detects the source of truth.
    #[arg(long, value_enum, default_value_t = RulesTarget::Auto)]
    pub rules_target: RulesTarget,

    /// Skip rules injection entirely (config + gitignore only).
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
}

/// One planned filesystem effect, collected before anything is written so `--print` can report a
/// faithful dry-run and the real run reports the same set.
enum Change {
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

    let mut changes = Vec::new();
    changes.push(plan_config(root)?);
    changes.push(plan_gitignore(root)?);
    if let Some(rule_change) = plan_rules(root, args, &caps, sections)? {
        changes.push(rule_change);
    }

    if args.print {
        report_dry_run(&changes);
        return Ok(());
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

/// Resolve the selected capability set from flags + interactivity.
fn select_capabilities(args: &InitArgs) -> Result<Vec<Capability>> {
    let with = parse_capabilities(&args.with)?;
    let without = parse_capabilities(&args.without)?;

    // Explicit `--with` is an allow-list; otherwise start from "all on".
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
        // EOF mid-prompt: accept the default for this and every remaining capability.
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
fn plan_config(root: &Path) -> Result<Change> {
    let path = config::config_path(root);
    if path.exists() {
        return Ok(Change::NoOp {
            note: format!("basemind.toml: kept existing config at {}", path.display()),
        });
    }
    let legacy = config::legacy_config_path(root);
    if legacy.exists() {
        // A root scaffold would silently shadow the legacy config (root path wins in resolution), so
        // refuse rather than change the effective config behind the user's back. They migrate the file,
        // then re-run init. Matches the pre-onboarding `cmd_init` contract (config_root_smoke).
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

/// Plan the `.gitignore` write: append `.basemind/` when the entry is missing.
fn plan_gitignore(root: &Path) -> Result<Change> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
    };
    let already = existing.as_deref().is_some_and(|contents| {
        contents.lines().any(|line| {
            let entry = line.trim().trim_start_matches('/').trim_end_matches('/');
            entry == ".basemind"
        })
    });
    if already {
        return Ok(Change::NoOp {
            note: format!(".gitignore: .basemind already ignored ({})", path.display()),
        });
    }
    let mut contents = existing.unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(".basemind/\n");
    Ok(Change::Write {
        path,
        note: "updated .gitignore (.basemind/)",
        contents,
    })
}

/// Which agent-instructions file owns the rules, after resolving `--rules-target`/`--no-rules`
/// against the detection priority.
enum RulesPlan {
    /// Skip rules entirely.
    Skip,
    /// Write an ai-rulez rule file at this path.
    AiRulez(PathBuf),
    /// Splice the delimited block into this markdown file (creating it if absent).
    Delimited(PathBuf),
}

/// Resolve where the rules go. Detection priority for `auto`:
/// `.ai-rulez/config.toml` → CLAUDE.md → AGENTS.md → create CLAUDE.md.
fn resolve_rules_plan(root: &Path, args: &InitArgs) -> RulesPlan {
    if args.no_rules || args.rules_target == RulesTarget::None {
        return RulesPlan::Skip;
    }
    let ai_rulez_rule = root.join(".ai-rulez").join("rules").join("basemind-usage.md");
    let claude = root.join("CLAUDE.md");
    let agents = root.join("AGENTS.md");
    match args.rules_target {
        RulesTarget::AiRulez => RulesPlan::AiRulez(ai_rulez_rule),
        RulesTarget::Claude => RulesPlan::Delimited(claude),
        RulesTarget::Agents => RulesPlan::Delimited(agents),
        RulesTarget::None => RulesPlan::Skip,
        RulesTarget::Auto => {
            if root.join(".ai-rulez").join("config.toml").exists() {
                RulesPlan::AiRulez(ai_rulez_rule)
            } else if claude.exists() {
                RulesPlan::Delimited(claude)
            } else if agents.exists() {
                RulesPlan::Delimited(agents)
            } else {
                RulesPlan::Delimited(claude)
            }
        }
    }
}

/// Plan the rules write. Returns `None` only when rules are skipped.
fn plan_rules(root: &Path, args: &InitArgs, caps: &[Capability], sections: BlockSections) -> Result<Option<Change>> {
    match resolve_rules_plan(root, args) {
        RulesPlan::Skip => Ok(Some(Change::NoOp {
            note: "rules: skipped (--no-rules / --rules-target none)".to_string(),
        })),
        RulesPlan::AiRulez(path) => {
            let contents = init_rules::render_ai_rulez_rule(caps, sections);
            let unchanged = std::fs::read_to_string(&path).is_ok_and(|prev| prev == contents);
            if unchanged {
                return Ok(Some(Change::NoOp {
                    note: format!("rules: ai-rulez rule already up to date ({})", path.display()),
                }));
            }
            Ok(Some(Change::Write {
                path,
                note: "wrote ai-rulez rule (run `ai-rulez generate` to render outputs)",
                contents,
            }))
        }
        RulesPlan::Delimited(path) => {
            let block = format!(
                "{BEGIN_MARKER}\n\n{}{END_MARKER}\n",
                init_rules::render_block_body(caps, sections)
            );
            let existing = match std::fs::read_to_string(&path) {
                Ok(c) => Some(c),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
            };
            let next = splice_block(existing.as_deref(), &block);
            if existing.as_deref() == Some(next.as_str()) {
                return Ok(Some(Change::NoOp {
                    note: format!("rules: block already up to date ({})", path.display()),
                }));
            }
            Ok(Some(Change::Write {
                path,
                note: "injected basemind rules block",
                contents: next,
            }))
        }
    }
}

/// Splice the managed `block` into `existing` markdown idempotently: replace between the markers if
/// present, else append at EOF. Content outside the markers is preserved verbatim.
fn splice_block(existing: Option<&str>, block: &str) -> String {
    let Some(existing) = existing else {
        return block.to_string();
    };
    if let (Some(begin), Some(end_start)) = (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        let end = end_start + END_MARKER.len();
        // Absorb a single trailing newline after the END marker so replacement is byte-stable.
        let tail_start = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..begin]);
        out.push_str(block);
        out.push_str(&existing[tail_start..]);
        return out;
    }
    // No markers: append at EOF with a blank-line separator.
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    out
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

    #[test]
    fn splice_appends_block_when_no_markers() {
        let out = splice_block(
            Some("# Title\n\nbody\n"),
            "<!-- BEGIN basemind (managed by `basemind init`) -->\nX\n<!-- END basemind -->\n",
        );
        assert!(out.starts_with("# Title\n\nbody\n"), "user content preserved");
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn splice_replaces_in_place_and_is_idempotent() {
        let block = format!("{BEGIN_MARKER}\nv2\n{END_MARKER}\n");
        let before = format!("intro\n\n{BEGIN_MARKER}\nv1\n{END_MARKER}\n\noutro\n");
        let after = splice_block(Some(&before), &block);
        assert_eq!(after.matches(BEGIN_MARKER).count(), 1, "no duplicate block");
        assert!(after.contains("v2") && !after.contains("v1"), "content replaced");
        assert!(
            after.contains("intro") && after.contains("outro"),
            "surrounding content kept"
        );
        // Re-splicing the same block is a fixpoint.
        assert_eq!(splice_block(Some(&after), &block), after);
    }

    #[test]
    fn none_target_skips_rules() {
        let args = InitArgs {
            rules_target: RulesTarget::None,
            ..Default::default()
        };
        assert!(matches!(
            resolve_rules_plan(Path::new("/nonexistent"), &args),
            RulesPlan::Skip
        ));
    }
}
