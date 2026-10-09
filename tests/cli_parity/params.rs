//! Layer 2: parameter parity between a CLI command and the MCP mode it is paired with.
//!
//! For each `pair` row the MCP side is the set of fields the mode accepts (probed from the real
//! validator, see `mcp_surface`; domain-wide for the modes that cannot be probed safely) and the CLI
//! side is the command's non-global arguments. Names are compared after snake-casing. Every
//! difference is a [`Violation`] that must be covered by a reviewed [`Exception`] carrying a reason;
//! an exception that no longer matches anything is itself a failure, so the table only ever shrinks
//! as the surfaces converge.

use std::collections::{BTreeMap, BTreeSet};

use crate::cli_tree::{CliArg, CliCommand};
use crate::mcp_surface::{Domain, Field, Kind, ModeParams};

/// What kind of difference an exception waives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Diff {
    /// The CLI argument has no MCP field of that name (or the mode rejects it).
    CliOnly,
    /// The mode accepts an MCP field the command has no argument for.
    McpOnly,
    /// The CLI argument and MCP field are the same parameter under different names/senses.
    Alias(&'static str),
    /// Switch / scalar / list shape differs.
    Shape,
    /// Allowed-value sets differ (or only one side constrains them).
    Values,
    /// Both sides declare a default and they differ.
    Default,
    /// One side requires the parameter and the other does not.
    Required,
}

/// A reviewed, reasoned waiver. `cli` is a command path, `"*"` (every pair), or `"<prefix> *"`.
#[derive(Debug, Clone, Copy)]
pub struct Exception {
    pub cli: &'static str,
    /// The CLI argument id (snake_case) — or the MCP field name for [`Diff::McpOnly`].
    pub param: &'static str,
    pub diff: Diff,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    pub cli: String,
    pub param: String,
    pub diff: Diff,
    pub detail: String,
}

fn scope_matches(scope: &str, cli: &str) -> bool {
    scope == "*"
        || scope == cli
        || scope
            .strip_suffix(" *")
            .is_some_and(|p| cli.starts_with(&format!("{p} ")))
}

impl Exception {
    fn covers(&self, v: &Violation) -> bool {
        self.param == v.param
            && scope_matches(self.cli, &v.cli)
            && match (self.diff, v.diff) {
                (Diff::Alias(_), Diff::CliOnly | Diff::McpOnly) => true,
                (a, b) => a == b,
            }
    }
}

fn shape_ok(cli: &CliArg, mcp: &Field) -> bool {
    let bool_valued = cli.possible_values.len() == 2 && cli.possible_values.iter().all(|v| v == "true" || v == "false");
    match mcp.kind {
        Kind::Bool => cli.switch || bool_valued,
        // A repeatable flag (`--env K=V`, `--tag t`) renders exactly like a scalar in `-h`.
        Kind::List => !cli.switch,
        _ => !cli.switch && !cli.variadic,
    }
}

fn values_ok(cli: &CliArg, mcp: &Field) -> bool {
    let bool_valued = cli.possible_values.iter().all(|v| v == "true" || v == "false");
    if bool_valued && mcp.enum_values.is_empty() {
        return true;
    }
    let a: BTreeSet<_> = cli.possible_values.iter().collect();
    let b: BTreeSet<_> = mcp.enum_values.iter().collect();
    a == b
}

/// Compare one pair. `probed` is `None` for modes compared domain-wide (no accepted/required sets).
pub fn compare(
    cmd: &CliCommand,
    domain: &Domain,
    probed: Option<&ModeParams>,
    exceptions: &[Exception],
) -> Vec<Violation> {
    let aliases: BTreeMap<&'static str, &'static str> = exceptions
        .iter()
        .filter(|e| scope_matches(e.cli, &cmd.path))
        .filter_map(|e| match e.diff {
            Diff::Alias(mcp) => Some((e.param, mcp)),
            _ => None,
        })
        .collect();
    let violation = |param: &str, diff, detail: String| Violation {
        cli: cmd.path.clone(),
        param: param.to_string(),
        diff,
        detail,
    };

    let mut out = Vec::new();
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    for arg in &cmd.args {
        let target = aliases.get(arg.id.as_str()).copied().unwrap_or(arg.id.as_str());
        let Some(field) = domain.fields.get(target) else {
            out.push(violation(
                &arg.id,
                Diff::CliOnly,
                format!("`{}` has no MCP field `{target}`", arg.display),
            ));
            continue;
        };
        if probed.is_some_and(|p| !p.accepted.contains(target)) {
            out.push(violation(
                &arg.id,
                Diff::CliOnly,
                format!("`{}` maps to `{target}`, which the MCP mode rejects", arg.display),
            ));
            continue;
        }
        matched.insert(target);
        if target != arg.id {
            // Renamed or sense-inverted on purpose: the waiver records it; shape/values/default
            // legitimately differ (`--no-index` switch vs `index` bool), so only required-ness is
            // still compared below.
            out.push(violation(
                &arg.id,
                Diff::Alias(target_static(&aliases, &arg.id)),
                format!("`{}` is the CLI spelling of MCP `{target}`", arg.display),
            ));
        } else if !shape_ok(arg, field) {
            out.push(violation(
                &arg.id,
                Diff::Shape,
                format!(
                    "`{}` is {} but MCP `{target}` is {:?}",
                    arg.display,
                    shape_name(arg),
                    field.kind
                ),
            ));
        }
        if target == arg.id && !values_ok(arg, field) {
            out.push(violation(
                &arg.id,
                Diff::Values,
                format!(
                    "CLI values {:?} vs MCP enum {:?}",
                    arg.possible_values, field.enum_values
                ),
            ));
        }
        if let (Some(c), Some(m)) = (&arg.default, &field.default)
            && target == arg.id
            && c != m
        {
            out.push(violation(
                &arg.id,
                Diff::Default,
                format!("CLI default `{c}` vs MCP default `{m}`"),
            ));
        }
        if let Some(p) = probed {
            let (cli_req, mcp_req) = (arg.required, p.required.contains(target));
            if cli_req != mcp_req {
                out.push(violation(
                    &arg.id,
                    Diff::Required,
                    format!("CLI required={cli_req}, MCP required={mcp_req}"),
                ));
            }
        }
    }
    if let Some(p) = probed {
        for field in p.accepted.iter().filter(|f| !matched.contains(f.as_str())) {
            out.push(violation(
                field,
                Diff::McpOnly,
                format!("MCP accepts `{field}`; the command has no matching argument"),
            ));
        }
    }
    out
}

fn target_static(aliases: &BTreeMap<&'static str, &'static str>, id: &str) -> &'static str {
    aliases[id]
}

fn shape_name(arg: &CliArg) -> &'static str {
    if arg.switch {
        "a switch"
    } else if arg.variadic {
        "a list"
    } else {
        "a scalar"
    }
}

/// The Rust line a developer pastes into `exceptions.rs` to waive `v`.
pub fn suggestion(v: &Violation) -> String {
    let (ctor, extra) = match v.diff {
        Diff::CliOnly => ("cli_only", String::new()),
        Diff::McpOnly => ("mcp_only", String::new()),
        Diff::Alias(m) => ("alias", format!("\"{m}\", ")),
        Diff::Shape => ("shape", String::new()),
        Diff::Values => ("values", String::new()),
        Diff::Default => ("default", String::new()),
        Diff::Required => ("required", String::new()),
    };
    format!("{ctor}(\"{}\", \"{}\", {extra}\"<reason>\"),", v.cli, v.param)
}

/// Check `violations` against `exceptions`; returns human-readable failures (empty = parity holds).
///
/// `compared` lists the CLI paths that were actually compared: an exception scoped to commands that
/// are not compiled into this build (feature-gated) cannot be judged stale.
pub fn reconcile(violations: &[Violation], exceptions: &[Exception], compared: &[String]) -> Vec<String> {
    let mut failures = Vec::new();
    for e in exceptions {
        if e.reason.trim().is_empty() {
            failures.push(format!(
                "exception for `{}` `{}` has an empty reason; every waiver must say why",
                e.cli, e.param
            ));
        }
    }
    let uncovered: BTreeSet<&Violation> = violations
        .iter()
        .filter(|v| !exceptions.iter().any(|e| e.covers(v)))
        .collect();
    for v in uncovered {
        failures.push(format!(
            "parameter mismatch in `basemind {}` on `{}`: {}\n    fix the CLI/MCP surface, or if intentional add to tests/cli_parity/exceptions.rs:\n    {}",
            v.cli, v.param, v.detail, suggestion(v)
        ));
    }
    for e in exceptions {
        if compared.iter().any(|c| scope_matches(e.cli, c)) && !violations.iter().any(|v| e.covers(v)) {
            failures.push(format!(
                "stale exception: `{}` `{}` ({:?}) no longer matches any mismatch; delete it from tests/cli_parity/exceptions.rs",
                e.cli, e.param, e.diff
            ));
        }
    }
    failures
}
