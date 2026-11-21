//! Usage errors in the spelling of the command that was typed.
//!
//! A tool body refuses its arguments by their MCP names (`` `code_grep`: `limit` must be at least
//! 1 ``). The CLI runs the same body, so [`super::run_tool`] wraps such a refusal in a
//! [`ToolUsage`], and [`respell`] rewrites it against the command's own clap definition: the tool
//! prefix becomes the command (`basemind code grep`), a field a flag sets becomes that flag
//! (`--limit`) and a field a positional sets becomes its value name (`<PATTERN>`).

use std::fmt;

use clap::{ArgMatches, Command};

use super::exit::CliExit;

/// A usage error a tool body raised, still in the tool's spelling. Exits `2`.
#[derive(Debug)]
pub struct ToolUsage {
    /// The MCP tool that refused the call, e.g. `code_grep`.
    pub tool: String,
    /// The tool's message, naming the tool's fields.
    pub message: String,
}

impl fmt::Display for ToolUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.tool, self.message)
    }
}

impl std::error::Error for ToolUsage {}

/// The command `matches` selected under `root`, with the names leading to it (`["code", "grep"]`).
pub fn selected<'a>(root: &'a Command, matches: &ArgMatches) -> (Vec<String>, &'a Command) {
    let mut path = Vec::new();
    let mut command = root;
    let mut matches = matches;
    while let Some((name, sub_matches)) = matches.subcommand() {
        let Some(sub) = command.find_subcommand(name) else {
            break;
        };
        path.push(sub.get_name().to_string());
        command = sub;
        matches = sub_matches;
    }
    (path, command)
}

/// Rewrite a [`ToolUsage`] at the top of `error` in the spelling of the command `matches` selected
/// under `root`; any other error is returned unchanged.
pub fn respell(error: anyhow::Error, root: &Command, matches: &ArgMatches) -> anyhow::Error {
    let Some(usage) = error.downcast_ref::<ToolUsage>() else {
        return error;
    };
    let (path, command) = selected(root, matches);
    let name = std::iter::once(root.get_name().to_string())
        .chain(path)
        .collect::<Vec<_>>()
        .join(" ");
    CliExit::usage(respelled(usage, &name, command, &supplied(matches, command)))
}

/// Every value the person typed for the command `matches` selected. A backquoted token equal to one
/// of them is read as that value, not as the field of the same name. Argument groups, which clap
/// reports with their members' names, are skipped.
fn supplied(matches: &ArgMatches, command: &Command) -> Vec<String> {
    let mut matches = matches;
    while let Some((_, sub_matches)) = matches.subcommand() {
        matches = sub_matches;
    }
    matches
        .ids()
        .filter(|id| command.get_arguments().any(|arg| arg.get_id() == id.as_str()))
        .filter(|id| matches.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine))
        .filter_map(|id| matches.get_raw(id.as_str()))
        .flatten()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

/// `usage` with its tool prefix replaced by `` `name` `` and every backquoted field of `command`
/// in its command-line spelling. The tool is spelled as on the wire (`code_deadcode`) or, by an
/// operation with no MCP tool, as the command group words it (`code deadcode`).
fn respelled(usage: &ToolUsage, name: &str, command: &Command, values: &[String]) -> String {
    let spaced = usage.tool.replace('_', " ");
    let is_tool = |quoted: &str| quoted == usage.tool || quoted == spaced;
    let body = [&usage.tool, &spaced]
        .iter()
        .find_map(|tool| usage.message.strip_prefix(&format!("`{tool}`: ")))
        .unwrap_or(&usage.message);
    let mut out = format!("`{name}`: ");
    let mut parts = body.split('`');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    let mut quoted = true;
    for part in parts {
        out.push('`');
        if quoted && !values.iter().any(|value| value == part) {
            match spelled(command, part) {
                Some(cli) => out.push_str(&cli),
                None if is_tool(part) => out.push_str(name),
                None => out.push_str(part),
            }
        } else {
            out.push_str(part);
        }
        quoted = !quoted;
    }
    out
}

/// The command-line spelling of the tool field `field` in `command`: `--flag` for a flag,
/// `<VALUE>` for a positional, `None` when no argument of `command` sets it.
fn spelled(command: &Command, field: &str) -> Option<String> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_global_set())
        .find_map(|arg| match arg.get_long() {
            Some(long) if long.replace('-', "_") == field || arg.get_id() == field => Some(format!("--{long}")),
            None if arg.is_positional() && arg.get_id() == field => Some(format!(
                "<{}>",
                arg.get_value_names()
                    .and_then(|names| names.first())
                    .map_or_else(|| field.to_uppercase(), ToString::to_string)
            )),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep() -> Command {
        Command::new("basemind").subcommand(
            Command::new("code").subcommand(
                Command::new("grep")
                    .arg(clap::Arg::new("pattern").value_name("PATTERN").required(true))
                    .arg(clap::Arg::new("max_tokens").long("max-tokens"))
                    .arg(clap::Arg::new("match_mode").long("match"))
                    .arg(clap::Arg::new("limit").long("limit")),
            ),
        )
    }

    fn respell_for(argv: &[&str], tool: &str, message: &str) -> String {
        let root = grep();
        let matches = root.clone().try_get_matches_from(argv).expect("parse");
        let error = anyhow::Error::new(ToolUsage {
            tool: tool.to_string(),
            message: message.to_string(),
        });
        let respelled = respell(error, &root, &matches);
        assert_eq!(super::super::exit::exit_code_for(&respelled), super::super::exit::USAGE);
        respelled.to_string()
    }

    #[test]
    fn a_tool_refusal_names_the_command_its_flags_and_its_positionals() {
        let argv = ["basemind", "code", "grep", "x"];
        assert_eq!(
            respell_for(
                &argv,
                "code_grep",
                "`code_grep`: `limit` must be at least 1; omit it for the default page size."
            ),
            "`basemind code grep`: `--limit` must be at least 1; omit it for the default page size."
        );
        assert_eq!(
            respell_for(&argv, "code_grep", "`max_tokens` 0 is below `pattern`'s floor"),
            "`basemind code grep`: `--max-tokens` 0 is below `<PATTERN>`'s floor"
        );
        assert_eq!(
            respell_for(&argv, "code_grep", "`code grep`: `limit` is 1 to 100"),
            "`basemind code grep`: `--limit` is 1 to 100"
        );
        assert_eq!(
            respell_for(
                &argv,
                "code_grep",
                "unknown `match` `fuzzy`; see `code_grep` and `docs_search`"
            ),
            "`basemind code grep`: unknown `--match` `fuzzy`; see `basemind code grep` and `docs_search`"
        );
    }

    #[test]
    fn a_quoted_value_the_person_typed_keeps_its_text_though_it_names_a_field() {
        let argv = ["basemind", "code", "grep", "limit", "--max-tokens", "0"];
        assert_eq!(
            respell_for(&argv, "code_grep", "`code_grep`: cannot resolve revision `limit`"),
            "`basemind code grep`: cannot resolve revision `limit`"
        );
        assert_eq!(
            respell_for(&argv, "code_grep", "`code_grep`: `max_tokens` is below the floor"),
            "`basemind code grep`: `--max-tokens` is below the floor"
        );
    }

    #[test]
    fn any_other_error_keeps_its_text_and_status() {
        let root = grep();
        let matches = root
            .clone()
            .try_get_matches_from(["basemind", "code", "grep", "x"])
            .expect("parse");
        let busy = respell(CliExit::busy("held"), &root, &matches);
        assert_eq!(busy.to_string(), "held");
        assert_eq!(super::super::exit::exit_code_for(&busy), super::super::exit::BUSY);
    }

    #[test]
    fn a_tool_usage_error_left_unrespelled_reads_as_before_and_exits_two() {
        let error = anyhow::Error::new(ToolUsage {
            tool: "code_outline".to_string(),
            message: "bad path".to_string(),
        });
        assert_eq!(error.to_string(), "code_outline: bad path");
        assert_eq!(super::super::exit::exit_code_for(&error), super::super::exit::USAGE);
    }
}
