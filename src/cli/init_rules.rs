//! Rules-block content generation for `basemind init`.
//!
//! The onboarding flow injects a "prefer basemind over grep/read/git" rule into the host repo's
//! agent-instructions file (CLAUDE.md / AGENTS.md / an ai-rulez rule). This module is the pure
//! content layer: given the selected [`Capability`] set and which optional sections are on, it
//! renders the markdown body. It performs no I/O — [`super::init`] owns file placement and the
//! idempotent delimited-block splice.
//!
//! The prose is condensed from `.ai-rulez/rules/agent-comms.md`: basemind first, shell/grep/git
//! is the fallback. Rows are emitted only for capabilities the user selected, so the advice never
//! advertises a tool the build/config can't serve.

use std::fmt::Write as _;

/// The onboarding capabilities. Each gates a routing row (and, for comms, a red-flag line) in the
/// rendered rules block. Order here is the display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// `search_symbols` / `find_references` / `find_callers` / `workspace_grep` — code navigation.
    CodeSearchNavigation,
    /// `outline` / `architecture_map` — file + repo structure without opening source.
    CodeMappingArchitecture,
    /// `recent_changes` / `blame_symbol` / `commits_touching` — git history without shelling out.
    GitHistory,
    /// `find_files` — fuzzy fzf/fd-style filename / path search.
    FileFinding,
    /// `thread_post` / `inbox_read` — multi-agent thread coordination.
    AgentComms,
    /// `workspaces` / `worktrees` / `worktree_claim` — daemon registry + worktree coordination.
    WorktreeCoordination,
    /// `search_documents` — RAG over PDFs / Office / HTML / the web.
    DocumentsRag,
    /// Semantic (vector) code search over the same index.
    SemanticSearch,
}

impl Capability {
    /// Every capability, in display order. The canonical selection universe.
    pub const ALL: [Capability; 8] = [
        Capability::CodeSearchNavigation,
        Capability::CodeMappingArchitecture,
        Capability::GitHistory,
        Capability::FileFinding,
        Capability::AgentComms,
        Capability::WorktreeCoordination,
        Capability::DocumentsRag,
        Capability::SemanticSearch,
    ];

    /// The stable CLI slug (`--with-<slug>` / `--without-<slug>`), also used in prompts.
    pub fn slug(self) -> &'static str {
        match self {
            Capability::CodeSearchNavigation => "code-search-navigation",
            Capability::CodeMappingArchitecture => "code-mapping-architecture",
            Capability::GitHistory => "git-history",
            Capability::FileFinding => "file-finding",
            Capability::AgentComms => "agent-comms",
            Capability::WorktreeCoordination => "worktree-coordination",
            Capability::DocumentsRag => "documents-rag",
            Capability::SemanticSearch => "semantic-search",
        }
    }

    /// Human label shown in the interactive prompt.
    pub fn label(self) -> &'static str {
        match self {
            Capability::CodeSearchNavigation => "Code search & navigation (symbols, references, callers, grep)",
            Capability::CodeMappingArchitecture => "Code mapping & architecture (file outlines, architecture map)",
            Capability::GitHistory => "Git history (recent changes, blame, commits-touching)",
            Capability::FileFinding => "File finding (fuzzy fzf/fd-style filename & path search)",
            Capability::AgentComms => "Agent comms (multi-agent threads + inbox)",
            Capability::WorktreeCoordination => {
                "Worktree coordination (daemon registry: workspaces, worktrees, advisory claims)"
            }
            Capability::DocumentsRag => "Documents & RAG (PDF / Office / HTML / web search)",
            Capability::SemanticSearch => "Semantic (vector) code search",
        }
    }

    /// Parse a slug back into a capability. Used by the `--with-`/`--without-` flag plumbing.
    pub fn from_slug(slug: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.slug() == slug)
    }

    /// The routing-table row: `(prefer this basemind tool, instead of this shell fallback)`.
    fn routing_row(self) -> (&'static str, &'static str) {
        match self {
            Capability::CodeSearchNavigation => (
                "`search_symbols` / `find_references` / `find_callers` / `workspace_grep`",
                "`grep` / `rg` / opening files to find a symbol",
            ),
            Capability::CodeMappingArchitecture => (
                "`outline` / `architecture_map`",
                "reading whole files to learn their shape",
            ),
            Capability::GitHistory => (
                "`recent_changes` / `blame_symbol` / `commits_touching` / `diff_file`",
                "`git log` / `git blame` / `git diff`",
            ),
            Capability::FileFinding => (
                "`find_files` (fuzzy path search)",
                "`find` / `fd` / `ls -R` to locate a file by name",
            ),
            Capability::AgentComms => (
                "`thread_post` / `inbox_read` / `thread_list`",
                "assuming you're the only agent in the repo",
            ),
            Capability::WorktreeCoordination => (
                "`workspaces` / `worktrees` / `worktree_claim`",
                "editing a worktree another session may already own",
            ),
            Capability::DocumentsRag => (
                "`search_documents` / `web_scrape` / `web_crawl` / `web_map`",
                "manually reading PDFs / docs or ad-hoc fetching",
            ),
            Capability::SemanticSearch => (
                "semantic code search over the index",
                "keyword-only guessing at where a concept lives",
            ),
        }
    }
}

/// Which optional parts of the block to render.
#[derive(Clone, Copy, Debug)]
pub struct BlockSections {
    /// Part (a): usage-priority prose + per-capability routing table + red-flags.
    pub usage_rules: bool,
    /// Part (b): setup & maintenance (marketplace install, auto-update, re-run init).
    pub setup_notes: bool,
}

/// Render the managed rules block body (WITHOUT the BEGIN/END delimiters — the caller wraps it).
///
/// `caps` must already be filtered to the user's selection; the rendered routing table and
/// red-flag list include only rows for those capabilities.
pub fn render_block_body(caps: &[Capability], sections: BlockSections) -> String {
    let mut out = String::new();
    out.push_str("## basemind — prefer it over grep / read / git\n\n");
    out.push_str(
        "basemind is this repo's indexed context layer. Prefer it BEFORE grep, before reading \
         files to find structure, and before naked `git` — it's the default, not a preference. \
         basemind returns paths, lines, and signatures at a fraction of the tokens of reading \
         source.\n\n",
    );

    if sections.usage_rules {
        render_usage_rules(&mut out, caps);
    }
    if sections.setup_notes {
        render_setup_notes(&mut out);
    }
    out
}

fn render_usage_rules(out: &mut String, caps: &[Capability]) {
    out.push_str("### Routing\n\n");
    out.push_str("| Reach for | Instead of |\n|---|---|\n");
    for cap in &Capability::ALL {
        if caps.contains(cap) {
            let (prefer, fallback) = cap.routing_row();
            let _ = writeln!(out, "| {prefer} | {fallback} |");
        }
    }
    out.push('\n');

    out.push_str("### Red flags — stop and re-route\n\n");
    if caps.contains(&Capability::CodeSearchNavigation) {
        out.push_str("- About to `grep` / `rg`? → `workspace_grep`.\n");
        out.push_str("- About to open a file just to find a symbol? → `outline` / `search_symbols`.\n");
    } else if caps.contains(&Capability::CodeMappingArchitecture) {
        out.push_str("- About to open a file just to learn its shape? → `outline`.\n");
    }
    if caps.contains(&Capability::GitHistory) {
        out.push_str("- About to `git log` / `git blame`? → `recent_changes` / `blame_symbol`.\n");
    }
    out.push_str("- Already mapped a file with basemind? Don't re-read it.\n\n");
}

fn render_setup_notes(out: &mut String) {
    out.push_str("### Setup & maintenance\n\n");
    out.push_str(
        "- Install the basemind Claude Code plugin from its marketplace \
         (`/plugin marketplace add Goldziher/basemind`, then install `basemind`).\n",
    );
    out.push_str(
        "- Keep basemind current: enable plugin auto-update, or update the binary regularly so \
         the index format and tools stay in sync.\n",
    );
    out.push_str(
        "- Re-run `basemind init` (or `/bm-init`) after enabling new capabilities to refresh this \
         block.\n\n",
    );
}

/// Render the full ai-rulez rule file (frontmatter + body). ai-rulez rules carry a `priority`
/// front-matter key; the body is the same content as the delimited block, sans delimiters.
pub fn render_ai_rulez_rule(caps: &[Capability], sections: BlockSections) -> String {
    let mut out = String::new();
    out.push_str("---\npriority: high\n---\n\n");
    out.push_str("# basemind usage\n\n");
    out.push_str(&render_block_body(caps, sections));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Capability> {
        Capability::ALL.to_vec()
    }

    #[test]
    fn from_slug_roundtrips_every_capability() {
        for cap in Capability::ALL {
            assert_eq!(Capability::from_slug(cap.slug()), Some(cap));
        }
        assert_eq!(Capability::from_slug("nope"), None);
    }

    #[test]
    fn routing_table_includes_only_selected_capabilities() {
        let sections = BlockSections {
            usage_rules: true,
            setup_notes: false,
        };
        let only_git = vec![Capability::GitHistory];
        let body = render_block_body(&only_git, sections);
        assert!(body.contains("`recent_changes`"), "git row present");
        assert!(
            !body.contains("`workspace_grep`"),
            "code-search row absent when unselected"
        );
    }

    #[test]
    fn setup_notes_toggle_controls_maintenance_section() {
        let with = render_block_body(
            &all(),
            BlockSections {
                usage_rules: false,
                setup_notes: true,
            },
        );
        assert!(with.contains("Setup & maintenance"));
        let without = render_block_body(
            &all(),
            BlockSections {
                usage_rules: false,
                setup_notes: false,
            },
        );
        assert!(!without.contains("Setup & maintenance"));
    }
}
