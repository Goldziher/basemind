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
    /// `code` modes `symbols` / `references` / `callers` / `grep` — code navigation.
    CodeSearchNavigation,
    /// `code` mode `outline` / `graph` mode `map` — file + repo structure without opening source.
    CodeMappingArchitecture,
    /// `git` modes `recent` / `blame_symbol` / `touching` — git history without shelling out.
    GitHistory,
    /// `code` mode `find` — fuzzy fzf/fd-style filename / path search.
    FileFinding,
    /// `agents` modes `post` / `inbox` — multi-agent thread coordination.
    AgentComms,
    /// `workspace` modes — daemon registry + worktree coordination.
    WorktreeCoordination,
    /// `memory` mode `documents` — RAG over PDFs / Office / HTML / markdown / config / the web.
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

    /// The routing-table row: `(Basemind tool, task)`.
    fn routing_row(self) -> (&'static str, &'static str) {
        match self {
            Capability::CodeSearchNavigation => (
                "`code` modes `symbols` / `definition` / `references` / `callers` / `implementations` / `grep`, `graph` mode `calls`",
                "Find definitions, references, callers, implementations, or exact text in indexed code",
            ),
            Capability::CodeMappingArchitecture => (
                "`code` mode `outline` (code files only) / `graph` mode `map`",
                "Inspect a file outline, call graph, or repository architecture",
            ),
            Capability::GitHistory => (
                "`git` modes `recent` / `blame_symbol` / `touching` / `diff`",
                "Inspect history, blame, or diffs",
            ),
            Capability::FileFinding => (
                "`code` mode `find` (fuzzy path search)",
                "Find files by name or path fragment",
            ),
            Capability::AgentComms => (
                "`agents` (modes `post` / `inbox` / `thread_list`)",
                "Discover threads, check the inbox, or coordinate with agents",
            ),
            Capability::WorktreeCoordination => (
                "`workspace` (modes `workspaces` / `worktrees` / `claim`)",
                "Inspect worktrees and check advisory claims before editing",
            ),
            Capability::DocumentsRag => (
                "`memory` mode `documents` (PDFs, Office, markdown, config and data files; the code map holds code only) / `web` (modes `scrape` / `crawl` / `map`)",
                "Search indexed documents or inspect web pages",
            ),
            Capability::SemanticSearch => (
                "`code` mode `semantic` (then `chunk`)",
                "Find code by concept and retrieve the relevant chunk",
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
/// notebook guidance include only the selected capabilities.
pub fn render_block_body(caps: &[Capability], sections: BlockSections) -> String {
    let mut out = String::new();
    out.push_str("## Basemind\n\n");

    if sections.usage_rules {
        render_usage_rules(&mut out, caps);
    }
    if sections.setup_notes {
        render_setup_notes(&mut out);
    }
    out
}

fn render_usage_rules(out: &mut String, caps: &[Capability]) {
    out.push_str(
        "**Use Basemind first for every capability listed below.** Make the Basemind MCP call \
         before using a shell command, file read, or another search tool for the same question. \
         Use the result to narrow any follow-up reads to the paths and lines you need.\n\n",
    );
    out.push_str("| First call | Task |\n|---|---|\n");
    for cap in &Capability::ALL {
        if caps.contains(cap) {
            let (tool, task) = cap.routing_row();
            let _ = writeln!(out, "| {tool} | {task} |");
        }
    }
    out.push('\n');

    out.push_str(
        "Use a fallback when the capability is unavailable, the index is stale, or the input is \
         outside its coverage. An empty indexed result only describes the indexed scope. \
         For an exhaustive search, verify uncovered paths with an exact file search.\n\n",
    );
    if caps.contains(&Capability::DocumentsRag) {
        out.push_str(
            "Use `memory` mode `documents` for semantic questions about notebooks and other \
             indexed documents. For an exact literal in a notebook, search the file text directly.\n\n",
        );
    }
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
        assert!(body.contains("Use Basemind first for every capability listed below"));
        assert!(body.contains("`git` modes `recent`"), "git row present");
        assert!(!body.contains("exact literal in a notebook"));
        assert!(
            !body.contains("`grep`, `graph`"),
            "code-search row absent when unselected"
        );
    }

    #[test]
    fn document_rules_distinguish_semantic_and_exact_notebook_search() {
        let body = render_block_body(
            &[Capability::DocumentsRag],
            BlockSections {
                usage_rules: true,
                setup_notes: false,
            },
        );
        assert!(body.contains("An empty indexed result only describes the indexed scope"));
        assert!(body.contains("semantic questions about notebooks"));
        assert!(body.contains("For an exact literal in a notebook, search the file text directly"));
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
        assert!(!with.contains("Use Basemind first"));
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
