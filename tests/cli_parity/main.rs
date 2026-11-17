//! Executable CLI<->MCP parity guard, in three layers (ADR-0014 as amended by ADR-0018).
//!
//! basemind's contract is that every tool an agent can call over MCP has a CLI command that runs
//! the same body with the same parameters and gives the same answer. The mapping is injective, not
//! a bijection: the CLI may also carry shorthands for one shape of a tool and administrative
//! commands with no MCP tool. The guard enforces it:
//!
//! 1. **Coverage** (`capabilities.rs`): one checked-in table maps each MCP tool to the one CLI
//!    command that is its counterpart, and classifies every other CLI command as a `shorthand` of a tool or as
//!    `cli_only` with a required reason. The test enumerates the live MCP surface (tool schemas)
//!    and the live command tree (the built binary's `-h` output) and fails on any uncovered tool or
//!    command, stale row, empty reason or duplicate mapping, with the exact row to add or delete.
//! 2. **Parameters** (`params.rs`, `exceptions.rs`): for each mapped pair, the fields the tool's
//!    schema accepts and requires are compared with the command's arguments: names,
//!    switch/scalar/list shape, allowed values, defaults and required-ness. A shorthand's
//!    arguments must all be fields of its tool. Every difference needs a reasoned entry in the
//!    exceptions table; entries that no longer match fail as stale.
//! 3. **Behaviour** (`behaviour.rs`): representative read-only queries run through the in-process
//!    MCP server and through `basemind --json` over one scanned fixture repo and must agree.

mod behaviour;
mod capabilities;
mod cli_tree;
mod exceptions;
mod fixture;
mod mcp_surface;
mod params;

use std::collections::{BTreeMap, BTreeSet};

use capabilities::{McpOp, Row};
use serde_json::Value;

fn row_text(row: &Row) -> String {
    match row {
        Row::Pair { mcp, cli } => format!("pair `{}` <-> `{cli}`", mcp.label()),
        Row::Shorthand { mcp, cli, .. } => format!("shorthand `{cli}` of `{}`", mcp.label()),
        Row::CliOnly { cli, .. } => format!("cli_only `{cli}`"),
        Row::McpOnly { mcp, .. } => format!("mcp_only `{}`", mcp.label()),
    }
}

/// Every operation the live MCP surface advertises, from the tool schemas: one per verb tool, one
/// per mode of a mode-dispatched domain tool.
fn live_mcp_ops(domains: &BTreeMap<String, mcp_surface::Domain>) -> BTreeSet<McpOp> {
    let leak = |text: &str| -> &'static str { Box::leak(text.to_string().into_boxed_str()) };
    let mut ops = BTreeSet::new();
    for (tool, domain) in domains {
        if domain.modes.is_empty() {
            ops.insert(McpOp {
                tool: leak(tool),
                mode: "",
            });
        }
        for mode in &domain.modes {
            ops.insert(McpOp {
                tool: leak(tool),
                mode: leak(mode),
            });
        }
    }
    ops
}

#[test]
fn capability_table_is_well_formed() {
    let rows = capabilities::table();
    let mut failures = Vec::new();
    let (mut seen_cli, mut seen_mcp) = (BTreeSet::new(), BTreeSet::new());
    for row in &rows {
        if row.reason().is_some_and(|r| r.trim().is_empty()) {
            failures.push(format!(
                "`{}` has an empty reason; shorthand/cli_only/mcp_only rows must say why",
                row_text(row)
            ));
        }
        if let Some(cli) = row.cli()
            && !seen_cli.insert(cli)
        {
            failures.push(format!(
                "CLI command `{cli}` appears in more than one row; keep exactly one"
            ));
        }
        if let Some(mcp) = row.mcp()
            && !seen_mcp.insert(mcp)
        {
            failures.push(format!(
                "MCP operation `{}` is paired with more than one CLI command; keep one `pair` and make the \
                 others `shorthand` rows",
                mcp.label()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "capability table problems:\n  {}",
        failures.join("\n  ")
    );
}

/// Layer 1, MCP side: every advertised tool (and every mode of a domain tool) has exactly one
/// counterpart row, every row names a real operation, the schemas agree with the server's compiled
/// vocabulary, and every shorthand points at a tool that exists.
#[test]
fn every_mcp_tool_is_covered() {
    let domains = mcp_surface::domains();
    let live = live_mcp_ops(&domains);

    #[allow(unused_mut)] // ~keep only a `comms` build adds the agent tools
    let mut compiled: BTreeSet<(String, String)> = basemind::mcp::params::VERB_TOOLS
        .iter()
        .map(|tool| (tool.to_string(), String::new()))
        .collect();
    #[cfg(all(feature = "comms", any(unix, windows)))]
    compiled.extend(
        basemind::mcp::agent_tools::AGENT_TOOLS
            .iter()
            .map(|tool| (tool.to_string(), String::new())),
    );
    let from_schema: BTreeSet<(String, String)> =
        live.iter().map(|o| (o.tool.to_string(), o.mode.to_string())).collect();
    assert_eq!(
        from_schema, compiled,
        "the advertised tool schemas disagree with VERB_TOOLS + AGENT_TOOLS"
    );

    let rows = capabilities::table();
    let mapped: BTreeSet<McpOp> = rows.iter().filter_map(Row::mcp).collect();
    let mut failures = Vec::new();
    for op in live.difference(&mapped) {
        failures.push(format!(
            "MCP `{}` has no capability row. Every MCP tool needs a CLI command that runs the same body: add the \
             command and a `tool(\"{}\", \"<cli path>\")` row to tests/cli_parity/capabilities.rs",
            op.label(),
            op.tool
        ));
    }
    for op in mapped.difference(&live) {
        failures.push(format!(
            "capability row for MCP `{}` is stale: the server no longer advertises it. Delete or rename the row \
             in tests/cli_parity/capabilities.rs",
            op.label()
        ));
    }
    for row in &rows {
        if let Row::Shorthand { mcp, cli, .. } = row
            && !live.contains(mcp)
        {
            failures.push(format!(
                "shorthand `{cli}` names MCP `{}`, which the server does not advertise",
                mcp.label()
            ));
        }
    }
    assert!(failures.is_empty(), "MCP coverage gaps:\n  {}", failures.join("\n  "));
}

/// Layer 1, CLI side: every leaf command has a row, every row names a real leaf command.
#[test]
fn every_cli_command_is_covered() {
    let tree = cli_tree::load();
    let live: BTreeSet<&str> = tree.leaves().map(|c| c.path.as_str()).collect();
    let rows = capabilities::table();
    let mapped: BTreeSet<&str> = rows.iter().filter_map(Row::cli).collect();
    let mut failures = Vec::new();
    for path in live.difference(&mapped) {
        failures.push(format!(
            "CLI command `basemind {path}` has no capability row. Add to tests/cli_parity/capabilities.rs:\n    cli_only(\"{path}\", \"<why there is no MCP tool>\"),\n    (or shorthand(\"<tool>\", \"{path}\", \"<the tool call it stands for>\") if it runs a tool's body)"
        ));
    }
    for path in mapped.difference(&live) {
        failures.push(format!(
            "capability row for CLI `{path}` is stale: no such leaf command in `basemind -h`. Rename the cli path in tests/cli_parity/capabilities.rs (renamed command) or delete the row"
        ));
    }
    assert!(failures.is_empty(), "CLI coverage gaps:\n  {}", failures.join("\n  "));
}

#[test]
fn global_flags_are_declared() {
    let tree = cli_tree::load();
    let declared: BTreeSet<&str> = exceptions::GLOBAL_FLAGS.iter().map(|(id, _)| *id).collect();
    let live: BTreeSet<&str> = tree.globals.iter().map(String::as_str).collect();
    assert_eq!(
        live, declared,
        "global CLI flags changed: update GLOBAL_FLAGS in tests/cli_parity/exceptions.rs (live = left, declared = right)"
    );
    assert!(
        exceptions::GLOBAL_FLAGS.iter().all(|(_, why)| !why.trim().is_empty()),
        "global flags need reasons"
    );
}

/// Layer 2. A `pair` is compared both ways against its tool's exact schema; a `shorthand` only has
/// to pass arguments that are fields of its tool.
#[test]
fn mapped_parameters_agree() {
    let tree = cli_tree::load();
    let domains = mcp_surface::domains();
    let rows = capabilities::table();
    let exceptions = exceptions::all();

    let (mut violations, mut compared) = (Vec::new(), Vec::new());
    let mut exact_pairs = 0usize;
    for row in &rows {
        let Some((mcp, cli)) = row.runs() else { continue };
        let (Some(cmd), Some(domain)) = (tree.commands.get(cli), domains.get(mcp.tool)) else {
            continue; // reported by the coverage tests
        };
        compared.push(cli.to_string());
        let exact = match row {
            Row::Pair { .. } => domain.exact_params(),
            _ => None,
        };
        exact_pairs += usize::from(exact.is_some());
        violations.extend(params::compare(cmd, domain, exact.as_ref(), &exceptions));
    }
    let tools = basemind::mcp::params::VERB_TOOLS.len()
        + if cfg!(all(feature = "comms", any(unix, windows))) {
            basemind::mcp::agent_tools::AGENT_TOOLS.len()
        } else {
            0
        };
    assert_eq!(
        exact_pairs, tools,
        "every tool must be compared against its exact schema"
    );
    let failures = params::reconcile(&violations, &exceptions, &compared);
    assert!(
        failures.is_empty(),
        "parameter parity failures:\n  {}",
        failures.join("\n  ")
    );
}

/// Layer 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_queries_agree_between_mcp_and_cli() {
    let session = mcp_surface::Session::start().await;
    let rows = capabilities::table();
    let mut failures = Vec::new();
    let (mut answered, mut refused, mut busy_beside) = (0usize, 0usize, 0usize);
    let cases = behaviour::cases();
    let mut compare = |case: &behaviour::Case, mcp: Result<Value, String>, cli: Result<Value, String>| {
        let label = format!("{} {} (basemind {})", case.tool, case.mcp, case.cli);
        match (mcp, cli) {
            (Ok(mut m), Ok(mut c)) => {
                behaviour::normalize(&mut m, case.volatile);
                behaviour::normalize(&mut c, case.volatile);
                if m != c {
                    failures.push(format!("{label}: payloads differ\n    mcp: {m}\n    cli: {c}"));
                }
                answered += 1;
            }
            // Both sides refuse (a tool whose feature is not in this build): parity is the CLI
            // printing the message the tool returned.
            (Err(m), Err(c)) if c.contains(&m) && m.contains("Cargo feature") => refused += 1,
            (m, c) => failures.push(format!("{label}: mcp={m:?} cli={c:?}")),
        }
    };

    let mut unmapped = Vec::new();
    let mut deferred = Vec::new();
    let mut beside_failures = Vec::new();
    for case in &cases {
        if !behaviour::table_pairs(&rows, case.tool, case.cli) {
            unmapped.push(format!(
                "{} {} (basemind {}): the capability table does not map this command to the tool",
                case.tool, case.mcp, case.cli
            ));
            continue;
        }
        let mcp = session.call(case.tool, case.mcp.clone()).await;
        if case.after_session {
            let beside = behaviour::run_cli(session.repo.path(), case.cli, case.cli_operands);
            match (&mcp, &beside) {
                // Not compiled in: the deferred comparison checks the CLI prints the same refusal.
                (Err(m), Err(c)) if c.contains(m.as_str()) && m.contains("Cargo feature") => {}
                _ if behaviour::refuses_beside_the_writer(&beside) => busy_beside += 1,
                _ => beside_failures.push(format!(
                    "{} {} (basemind {}) beside the writer with no daemon: expected a busy refusal \
                     (exit 3) naming `basemind comms start`, got {beside:?}",
                    case.tool, case.mcp, case.cli
                )),
            }
            deferred.push((case, mcp));
            continue;
        }
        let cli = behaviour::run_cli(session.repo.path(), case.cli, case.cli_operands);
        compare(case, mcp, cli);
    }
    if let Err(stderr) = behaviour::leaves_the_held_history_alone(session.repo.path(), "git log", &[]) {
        beside_failures.push(format!(
            "basemind git log beside the writer opened the git-history database it holds: {stderr}"
        ));
    }
    let repo = session.close().await;
    for (case, mcp) in deferred {
        let cli = behaviour::run_cli_once_unlocked(repo.path(), case.cli, case.cli_operands);
        compare(case, mcp, cli);
    }
    failures.extend(unmapped);
    failures.extend(beside_failures);

    assert!(
        cases.len() >= 27,
        "the behaviour layer lost its cases ({} left)",
        cases.len()
    );
    println!(
        "behavioural parity: {answered} answered on both sides, {refused} refused on both sides, \
         {busy_beside} refused busy beside the writer"
    );
    assert!(
        answered >= 22,
        "only {answered} cases compared payloads; the rest merely agreed to refuse"
    );
    assert!(
        failures.is_empty(),
        "behavioural parity failures:\n  {}",
        failures.join("\n  ")
    );
}
