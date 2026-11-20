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
    /// `code_symbols` / `code_refs` / `code_grep` / `code_read` — code navigation.
    CodeSearchNavigation,
    /// `code_outline` — file structure without opening source.
    CodeMappingArchitecture,
    /// `git_log` / `git_blame` / `git_diff` — git history without shelling out.
    GitHistory,
    /// `find_files` — fuzzy fzf/fd-style filename / path search.
    FileFinding,
    /// `agent_post` / `agent_inbox` / `agent_threads` — messaging other agents.
    AgentComms,
    /// `docs_search` — RAG over PDFs / Office / HTML / markdown / config.
    DocumentsRag,
    /// `code_search` — semantic (vector) code search over the same index.
    SemanticSearch,
}

impl Capability {
    /// Every capability, in display order. The canonical selection universe.
    pub const ALL: [Capability; 7] = [
        Capability::CodeSearchNavigation,
        Capability::CodeMappingArchitecture,
        Capability::GitHistory,
        Capability::FileFinding,
        Capability::AgentComms,
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
            Capability::DocumentsRag => "documents-rag",
            Capability::SemanticSearch => "semantic-search",
        }
    }

    /// Human label shown in the interactive prompt.
    pub fn label(self) -> &'static str {
        match self {
            Capability::CodeSearchNavigation => "Code search & navigation (symbols, usages, grep, reading a symbol)",
            Capability::CodeMappingArchitecture => "Code mapping & architecture (file outlines)",
            Capability::GitHistory => "Git history (log, blame, diff)",
            Capability::FileFinding => "File finding (fuzzy fzf/fd-style filename & path search)",
            Capability::AgentComms => "Agent comms (multi-agent threads + inbox)",
            Capability::DocumentsRag => "Documents & RAG (PDF / Office / HTML search)",
            Capability::SemanticSearch => "Semantic (vector) code search",
        }
    }

    /// Parse a slug back into a capability. Used by the `--with-`/`--without-` flag plumbing.
    pub fn from_slug(slug: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.slug() == slug)
    }

    /// The routing rows: `(task, example call)`, one per tool the capability covers. Each call is
    /// complete and valid as written, so an agent can adapt it instead of guessing arguments.
    fn routing_rows(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Capability::CodeSearchNavigation => &[
                ("Where is X defined", r#"`code_symbols {"name":"parse_config"}`"#),
                (
                    "What calls / implements / imports X",
                    r#"`code_refs {"name":"parse_config"}` (add `"relation":"implementations"` or `"importers"`)"#,
                ),
                (
                    "Find a string, identifier or comment in code, markdown, config or data files",
                    r#"`code_grep {"pattern":"cache_root("}` (literal; add `"regex":true` for a regex)"#,
                ),
                (
                    "Read one function or a line span",
                    r#"`code_read {"path":"src/store.rs","symbol":"open"}`"#,
                ),
            ],
            Capability::CodeMappingArchitecture => &[(
                "See what a code file contains without opening it",
                r#"`code_outline {"path":"src/store.rs"}`"#,
            )],
            Capability::GitHistory => &[
                (
                    "Recent commits, a file's history, or a commit search",
                    r#"`git_log {}` / `git_log {"path":"src/store.rs"}` / `git_log {"query":"fix race"}`"#,
                ),
                (
                    "Who last changed a symbol or line span",
                    r#"`git_blame {"path":"src/store.rs","symbol":"open"}`"#,
                ),
                (
                    "Working-tree status, or a file's diff",
                    r#"`git_diff {}` / `git_diff {"path":"src/store.rs"}`"#,
                ),
            ],
            Capability::FileFinding => &[(
                "Find a file by a fragment of its name or path",
                r#"`find_files {"query":"readme"}`"#,
            )],
            Capability::AgentComms => &[(
                "Read messages from other agents, find threads, or post to them",
                r#"`agent_inbox {}` / `agent_threads {"query":"…"}` / `agent_post {"subject":"…","body":"…"}`"#,
            )],
            Capability::DocumentsRag => &[(
                "Search documents by meaning (PDFs, Office, markdown, notebooks)",
                r#"`docs_search {"query":"how releases are versioned"}`"#,
            )],
            Capability::SemanticSearch => &[(
                "Find code by what it does, then read the hit",
                r#"`code_search {"query":"retry a failed upload with backoff"}`"#,
            )],
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
    out.push_str("## basemind\n\n");

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
        "**Use basemind first for every capability listed below.** Make the basemind MCP call \
         before using a shell command, file read, or another search tool for the same question. \
         Use the result to narrow any follow-up reads to the paths and lines you need.\n\n",
    );
    out.push_str(
        "The basemind tools are always loaded: call them directly, there is no tool search to run \
         first. The agent tools `agent_post`, `agent_inbox` and `agent_threads` are the exception: \
         they are deferred, so find them by tool search. Arguments are exact (an unknown argument is rejected, not ignored), paths are \
         repository-relative, and `line` / `end_line` are 1-based.\n\n",
    );
    out.push_str("| Task | First call |\n|---|---|\n");
    for cap in &Capability::ALL {
        if caps.contains(cap) {
            for (task, call) in cap.routing_rows() {
                let _ = writeln!(out, "| {task} | {call} |");
            }
        }
    }
    out.push_str(
        "\nWhen a result is unexpectedly empty call `index_status {}`. The index follows your edits: if a \
         result says `stale` or a new file is missing, call `index_rescan {\"paths\":[\"src/store.rs\"]}` \
         for the files you touched, and `index_rescan {}` only when `index_status` reports an empty or \
         degraded index.\n\n",
    );

    out.push_str(
        "Use a fallback when the capability is unavailable, the index is stale, or the input is \
         outside its coverage. An empty indexed result only describes the indexed scope. \
         For an exhaustive search, verify uncovered paths with an exact file search.\n\n",
    );
    if caps.contains(&Capability::DocumentsRag) {
        out.push_str(
            "Use `docs_search` for semantic questions about notebooks and other \
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

/// Render the full ai-rulez rule file (frontmatter + body) in the OKF shape `ai-rulez init`
/// writes: `type`/`title` plus an `x-ai-rulez` block holding the id and `metadata.priority`. The
/// body is the same content as the delimited block, sans delimiters.
pub fn render_ai_rulez_rule(caps: &[Capability], sections: BlockSections) -> String {
    let mut out = String::new();
    out.push_str("---\ntype: Decision\ntitle: basemind usage\nx-ai-rulez:\n  kind: rule\n  id: basemind-usage\n  metadata:\n    priority: high\n---\n\n");
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
    fn ai_rulez_rule_uses_the_okf_frontmatter_shape() {
        let rule = render_ai_rulez_rule(
            &all(),
            BlockSections {
                usage_rules: true,
                setup_notes: false,
            },
        );
        assert!(rule.starts_with(
            "---\ntype: Decision\ntitle: basemind usage\nx-ai-rulez:\n  kind: rule\n  id: basemind-usage\n  metadata:\n    priority: high\n---\n\n# basemind usage\n"
        ));
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
        assert!(body.contains("Use basemind first for every capability listed below"));
        assert!(body.contains("`git_log {}`"), "git row present");
        assert!(!body.contains("exact literal in a notebook"));
        assert!(
            !body.contains("code_symbols"),
            "code-search rows absent when unselected"
        );
    }

    /// Every capability contributes at least one complete example call, the block says the tools
    /// are always loaded, and nothing describes the retired mode-dispatched surface.
    #[test]
    fn every_capability_has_an_example_call_and_no_retired_tool_is_named() {
        let body = render_block_body(
            &all(),
            BlockSections {
                usage_rules: true,
                setup_notes: false,
            },
        );
        assert!(body.contains("always loaded"), "{body}");
        for cap in Capability::ALL {
            let rows = cap.routing_rows();
            assert!(!rows.is_empty(), "{cap:?} has no example call");
            for (_, call) in rows {
                assert!(
                    call.contains('{') && call.contains('}'),
                    "{cap:?} row has no call: {call}"
                );
                assert!(body.contains(call), "{cap:?} row missing from the block");
            }
        }
        for tool in [
            "code_outline",
            "code_symbols",
            "code_read",
            "code_grep",
            "code_search",
            "code_refs",
            "find_files",
            "docs_search",
            "git_log",
            "git_blame",
            "git_diff",
            "index_status",
            "index_rescan",
        ] {
            assert!(body.contains(tool), "the block never names `{tool}`");
        }
        for retired in [
            "`code` mode",
            "`git` mode",
            "`memory` mode",
            "`admin` mode",
            "`workspace`",
            "blame_symbol",
            "agents {",
            "thread_list",
            "mode\":",
        ] {
            assert!(!body.contains(retired), "the block still names `{retired}`");
        }
    }

    /// The daemon watches the workspace, so the block steers a rescan to the edited paths and keeps
    /// the whole-workspace rescan for an empty or degraded index.
    #[test]
    fn usage_rules_steer_a_rescan_to_the_edited_paths() {
        let body = render_block_body(
            &all(),
            BlockSections {
                usage_rules: true,
                setup_notes: false,
            },
        );
        assert!(
            body.contains(
                "if a result says `stale` or a new file is missing, call \
                 `index_rescan {\"paths\":[\"src/store.rs\"]}` for the files you touched"
            ),
            "{body}"
        );
        assert!(
            body.contains("`index_rescan {}` only when `index_status` reports an empty or degraded index"),
            "{body}"
        );
        assert!(!body.contains("after edits call `index_rescan {}`"), "{body}");
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
        assert!(!with.contains("Use basemind first"));
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
