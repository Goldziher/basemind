//! Executable CLI<->MCP parity guard, in three layers.
//!
//! basemind's contract is that every operation an agent can invoke over MCP is also reachable from
//! the `basemind` CLI, with the same parameters and the same answer. The guard enforces it:
//!
//! 1. **Coverage** (`capabilities.rs`): one checked-in table maps each MCP `tool`+`mode` to the CLI
//!    command that runs it, or declares the operation `cli_only` / `mcp_only` with a required
//!    reason. The test enumerates the live MCP surface (tool schemas) and the live command tree
//!    (the built binary's `-h` output) and fails on any uncovered mode or command, stale row, empty
//!    reason or duplicate mapping — with the exact row to add or delete.
//! 2. **Parameters** (`params.rs`, `exceptions.rs`): for each mapped pair, the fields the MCP mode
//!    accepts (probed from the server's own validator) are compared with the command's arguments —
//!    names, switch/scalar/list shape, allowed values, defaults and required-ness. Every difference
//!    needs a reasoned entry in the exceptions table; entries that no longer match fail as stale.
//! 3. **Behaviour** (`behaviour.rs`): representative read-only queries run through the in-process
//!    MCP server and through `basemind --json` over one scanned fixture repo and must agree.
//!
//! **Why `(tool, mode)` and not tool names.** The operations live in a required `mode` enum behind
//! a handful of domain tools, so a name-keyed table would silently stop covering them.

mod behaviour;
mod capabilities;
mod cli_tree;
mod exceptions;
mod fixture;
mod mcp_surface;
mod params;

use std::collections::{BTreeMap, BTreeSet};

use capabilities::{McpOp, Row};

fn row_text(row: &Row) -> String {
    match row {
        Row::Pair { mcp, cli } => format!("pair(\"{}\", \"{}\", \"{cli}\"),", mcp.tool, mcp.mode),
        Row::CliOnly { cli, .. } => format!("cli_only(\"{cli}\", \"<why there is no MCP counterpart>\"),"),
        Row::McpOnly { mcp, .. } => format!(
            "mcp_only(\"{}\", \"{}\", \"<why there is no CLI command>\"),",
            mcp.tool, mcp.mode
        ),
    }
}

/// Every `(tool, mode)` the live MCP surface advertises, from the tool schemas.
fn live_mcp_ops(domains: &BTreeMap<String, mcp_surface::Domain>) -> BTreeSet<McpOp> {
    let mut ops = BTreeSet::new();
    for (tool, domain) in domains {
        for mode in &domain.modes {
            ops.insert(McpOp {
                tool: Box::leak(tool.clone().into_boxed_str()),
                mode: Box::leak(mode.clone().into_boxed_str()),
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
                "`{}` has an empty reason; cli_only/mcp_only rows must say why",
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
                "MCP operation `{}:{}` appears in more than one row; keep exactly one",
                mcp.tool, mcp.mode
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "capability table problems:\n  {}",
        failures.join("\n  ")
    );
}

/// Layer 1, MCP side: every advertised mode has a row, every row names a real mode, and the schema
/// enums agree with the server's compiled `domain_modes()`.
#[test]
fn every_mcp_mode_is_covered() {
    let domains = mcp_surface::domains();
    let live = live_mcp_ops(&domains);

    let compiled: BTreeSet<(String, String)> = basemind::mcp::mode::domain_modes()
        .into_iter()
        .flat_map(|(tool, modes)| modes.iter().map(move |m| (tool.to_string(), m.to_string())))
        .collect();
    let from_schema: BTreeSet<(String, String)> =
        live.iter().map(|o| (o.tool.to_string(), o.mode.to_string())).collect();
    assert_eq!(
        from_schema, compiled,
        "tool schema `mode` enums disagree with mcp::mode::domain_modes()"
    );

    let rows = capabilities::table();
    let mapped: BTreeSet<McpOp> = rows.iter().filter_map(Row::mcp).collect();
    let mut failures = Vec::new();
    for op in live.difference(&mapped) {
        failures.push(format!(
            "MCP `{0}` mode `{1}` has no capability row. Add the CLI command and a row to tests/cli_parity/capabilities.rs:\n    pair(\"{0}\", \"{1}\", \"<cli path, e.g. {0} {2}>\"),\n    (or mcp_only(\"{0}\", \"{1}\", \"<reason>\") if it must stay MCP-only)",
            op.tool, op.mode, op.mode.replace('_', "-")
        ));
    }
    for op in mapped.difference(&live) {
        failures.push(format!(
            "capability row for MCP `{}:{}` is stale: the server no longer advertises it. Delete or rename the row in tests/cli_parity/capabilities.rs",
            op.tool, op.mode
        ));
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
            "CLI command `basemind {path}` has no capability row. Add to tests/cli_parity/capabilities.rs:\n    cli_only(\"{path}\", \"<why there is no MCP counterpart>\"),\n    (or point an existing/new MCP mode at it with pair(\"<tool>\", \"<mode>\", \"{path}\"))"
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

/// Layer 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapped_parameters_agree() {
    let tree = cli_tree::load();
    let domains = mcp_surface::domains();
    let session = mcp_surface::Session::start().await;
    let rows = capabilities::table();
    let exceptions = exceptions::all();

    let (mut violations, mut compared) = (Vec::new(), Vec::new());
    for row in &rows {
        let Row::Pair { mcp, cli } = row else { continue };
        let (Some(cmd), Some(domain)) = (tree.commands.get(*cli), domains.get(mcp.tool)) else {
            continue; // reported by the coverage tests
        };
        compared.push(cli.to_string());
        let probed = if mcp_surface::is_probed(mcp.tool, mcp.mode) {
            let p = session.probe(mcp.tool, mcp.mode, domain).await;
            if p.accepted.len() == domain.fields.len() {
                violations.push(params::Violation {
                    cli: cli.to_string(),
                    param: "<probe>".into(),
                    diff: params::Diff::CliOnly,
                    detail: format!("probe of `{}:{}` rejected nothing, so the validator did not run; mark it unprobed in mcp_surface.rs", mcp.tool, mcp.mode),
                });
                None
            } else {
                Some(p)
            }
        } else {
            None
        };
        violations.extend(params::compare(cmd, domain, probed.as_ref(), &exceptions));
    }
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
    for case in behaviour::cases() {
        let label = format!("{}:{}", case.op.tool, case.op.mode);
        let Some(cli_path) = behaviour::cli_path_for(&rows, case.op) else {
            failures.push(format!("{label}: no `pair` row in the capability table"));
            continue;
        };
        let mcp = session.call(case.op.tool, case.mcp.clone()).await;
        let cli = behaviour::run_cli(session.repo.path(), cli_path, case.cli_operands);
        match (mcp, cli) {
            (Ok(mut m), Ok(mut c)) => {
                behaviour::normalize(&mut m, case.unordered);
                behaviour::normalize(&mut c, case.unordered);
                if m != c {
                    failures.push(format!(
                        "{label} (basemind {cli_path}): payloads differ\n    mcp: {m}\n    cli: {c}"
                    ));
                }
            }
            (m, c) => failures.push(format!("{label} (basemind {cli_path}): mcp={m:?} cli={c:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "behavioural parity failures:\n  {}",
        failures.join("\n  ")
    );
}
