//! `--help-json`: a command's arguments as JSON, read from the same clap definition the binary
//! parses its arguments with (issue #332).
//!
//! A script or an agent checking a `basemind …` example needs what `--help` says in a form it can
//! read: how many positionals the command takes, each flag's names and aliases, and whether a flag
//! takes a value. [`requested`] answers before the arguments are parsed, so a command whose
//! required positionals are missing still describes itself, and nothing touches a workspace, the
//! cache or a daemon.
//!
//! The answer is one object, [`FORMAT`] `1`:
//!
//! - `command`: the command as typed (`basemind code grep`), `path` its names after `basemind`,
//!   `about` its one-line help, `aliases` its other names.
//! - `subcommands`: `name`, `aliases` and `about` of each subcommand.
//! - `positional_count`: the fewest and most positional values the command accepts (`max` `null`
//!   when unbounded), and `positionals`, each with `name`, `value_name`, `required`, `min_values`,
//!   `max_values`, `default`, `possible_values` and `help`.
//! - `flags`: each with `name`, `long` (`--max-tokens`), `short` (`-q`), `aliases`, `takes_value`,
//!   `value_name`, `required`, `repeatable`, `global`, `default`, `possible_values`, `env` and
//!   `help`.
//!
//! Hidden commands and arguments are left out, as `--help` leaves them out.

use std::ffi::OsString;

use clap::{Arg, ArgAction, Command};
use serde_json::{Value, json};

/// The version of the `--help-json` object; it grows when a field is added or changes meaning.
pub const FORMAT: u32 = 1;

/// The flag as typed.
const LONG: &str = "help-json";

/// The global `--help-json` flag, declared on the root command so every command lists it.
pub fn flag() -> Arg {
    Arg::new("help_json")
        .long(LONG)
        .global(true)
        .action(ArgAction::SetTrue)
        .help(
            "Print this command's arguments as JSON (positional counts, flags and their aliases, whether \
             each takes a value) and exit without running it",
        )
}

/// The description of the command `args` names under `root` when `args` (the process arguments,
/// program name first) hold `--help-json` before any `--`; `None` otherwise.
pub fn requested(root: &mut Command, args: &[OsString]) -> Option<Value> {
    let words: Vec<&OsString> = args
        .iter()
        .skip(1)
        .take_while(|word| word.as_os_str() != "--")
        .collect();
    let wanted = format!("--{LONG}");
    if !words.iter().any(|word| word.as_os_str() == wanted.as_str()) {
        return None;
    }
    root.build();
    let (path, command) = addressed(root, &words);
    Some(describe(root.get_name(), &path, command))
}

/// The command `words` address under `root`, and the names leading to it. A word that names no
/// subcommand is a positional value; a flag that takes a value consumes the word after it.
fn addressed<'a>(root: &'a Command, words: &[&OsString]) -> (Vec<String>, &'a Command) {
    let mut path = Vec::new();
    let mut command = root;
    let mut words = words.iter().map(|word| word.to_string_lossy());
    while let Some(word) = words.next() {
        let takes_next = if let Some(long) = word.strip_prefix("--") {
            !long.contains('=')
                && command
                    .get_arguments()
                    .any(|arg| names_long(arg, long) && takes_value(arg))
        } else if let Some(shorts) = word.strip_prefix('-').filter(|shorts| !shorts.is_empty()) {
            short_takes_next(command, shorts)
        } else {
            if let Some(sub) = command.find_subcommand(&*word) {
                path.push(sub.get_name().to_string());
                command = sub;
            }
            false
        };
        if takes_next {
            words.next();
        }
    }
    (path, command)
}

/// Whether a cluster of short flags (`qv`, `n5`) leaves its value to the next word: true when the
/// first flag in it that takes a value is its last character.
fn short_takes_next(command: &Command, shorts: &str) -> bool {
    let mut chars = shorts.chars().peekable();
    while let Some(short) = chars.next() {
        let named = command.get_arguments().find(|arg| {
            arg.get_short() == Some(short)
                || arg
                    .get_all_short_aliases()
                    .is_some_and(|aliases| aliases.contains(&short))
        });
        if named.is_some_and(takes_value) {
            return chars.peek().is_none();
        }
    }
    false
}

fn names_long(arg: &Arg, long: &str) -> bool {
    arg.get_long() == Some(long) || arg.get_all_aliases().unwrap_or_default().contains(&long)
}

fn takes_value(arg: &Arg) -> bool {
    arg.get_num_args().is_some_and(|range| range.takes_values())
}

/// A value count as JSON: `null` when unbounded.
fn bound(count: usize) -> Value {
    if count == usize::MAX { Value::Null } else { json!(count) }
}

fn text(styled: Option<&clap::builder::StyledStr>) -> Value {
    styled.map_or(Value::Null, |styled| Value::String(styled.to_string()))
}

fn possible_values(arg: &Arg) -> Vec<String> {
    if !takes_value(arg) {
        return Vec::new();
    }
    arg.get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_string())
        .collect()
}

fn defaults(arg: &Arg) -> Vec<String> {
    if !takes_value(arg) {
        return Vec::new();
    }
    arg.get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn value_name(arg: &Arg) -> Option<String> {
    if !takes_value(arg) {
        return None;
    }
    Some(
        arg.get_value_names()
            .and_then(|names| names.first())
            .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string),
    )
}

/// The fewest and most values `arg` takes; an argument that may repeat (`Vec` positionals) takes any
/// number of occurrences, so its most is unbounded.
fn value_counts(arg: &Arg) -> (usize, usize) {
    let (min, max) = arg
        .get_num_args()
        .map_or((0, 0), |range| (range.min_values(), range.max_values()));
    if matches!(arg.get_action(), ArgAction::Append) {
        (min, usize::MAX)
    } else {
        (min, max)
    }
}

fn positional(arg: &Arg) -> Value {
    let (min, max) = value_counts(arg);
    json!({
        "name": arg.get_id().as_str(),
        "value_name": value_name(arg),
        "required": arg.is_required_set(),
        "min_values": min,
        "max_values": bound(max),
        "default": defaults(arg),
        "possible_values": possible_values(arg),
        "help": text(arg.get_help()),
    })
}

fn flag_entry(arg: &Arg) -> Value {
    let longs = arg
        .get_visible_aliases()
        .unwrap_or_default()
        .into_iter()
        .map(|alias| format!("--{alias}"));
    let shorts = arg
        .get_visible_short_aliases()
        .unwrap_or_default()
        .into_iter()
        .map(|alias| format!("-{alias}"));
    json!({
        "name": arg.get_id().as_str(),
        "long": arg.get_long().map(|long| format!("--{long}")),
        "short": arg.get_short().map(|short| format!("-{short}")),
        "aliases": longs.chain(shorts).collect::<Vec<_>>(),
        "takes_value": takes_value(arg),
        "value_name": value_name(arg),
        "required": arg.is_required_set(),
        "repeatable": matches!(arg.get_action(), ArgAction::Append | ArgAction::Count),
        "global": arg.is_global_set(),
        "default": defaults(arg),
        "possible_values": possible_values(arg),
        "env": arg.get_env().map(|env| env.to_string_lossy().into_owned()),
        "help": text(arg.get_help()),
    })
}

/// The `--help-json` object of `command`, reached from the root `program` through `path`.
fn describe(program: &str, path: &[String], command: &Command) -> Value {
    let visible: Vec<&Arg> = command.get_arguments().filter(|arg| !arg.is_hide_set()).collect();
    let positionals: Vec<&Arg> = visible.iter().copied().filter(|arg| arg.is_positional()).collect();
    let min = positionals
        .iter()
        .filter(|arg| arg.is_required_set())
        .map(|arg| value_counts(arg).0)
        .sum::<usize>();
    let max = positionals
        .iter()
        .map(|arg| value_counts(arg).1)
        .try_fold(0usize, usize::checked_add)
        .unwrap_or(usize::MAX);
    let subcommands: Vec<Value> = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| {
            json!({
                "name": sub.get_name(),
                "aliases": sub.get_visible_aliases().collect::<Vec<_>>(),
                "about": text(sub.get_about()),
            })
        })
        .collect();
    json!({
        "format": FORMAT,
        "command": std::iter::once(program).chain(path.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        "path": path,
        "about": text(command.get_about()),
        "aliases": command.get_visible_aliases().collect::<Vec<_>>(),
        "subcommands": subcommands,
        "positional_count": { "min": min, "max": bound(max) },
        "positionals": positionals.iter().map(|arg| positional(arg)).collect::<Vec<_>>(),
        "flags": visible.iter().filter(|arg| !arg.is_positional()).map(|arg| flag_entry(arg)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> Command {
        Command::new("tool")
            .arg(flag())
            .arg(Arg::new("root").long("root").global(true).help("Repository root."))
            .arg(
                Arg::new("verbose")
                    .short('v')
                    .long("verbose")
                    .global(true)
                    .action(ArgAction::SetTrue)
                    .help("Say more."),
            )
            .subcommand(
                Command::new("scan")
                    .about("Index the tree.")
                    .arg(Arg::new("staged").long("staged").action(ArgAction::SetTrue)),
            )
            .subcommand(
                Command::new("rescan")
                    .visible_alias("rs")
                    .about("Re-index.")
                    .arg(Arg::new("paths").value_name("PATH").num_args(0..).help("Paths."))
                    .arg(
                        Arg::new("limit")
                            .long("limit")
                            .short('n')
                            .visible_alias("max")
                            .default_value("10")
                            .value_parser(["5", "10"]),
                    ),
            )
    }

    fn help(words: &[&str]) -> Option<Value> {
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        requested(&mut cli(), &args)
    }

    fn flag_named<'a>(help: &'a Value, name: &str) -> &'a Value {
        help["flags"]
            .as_array()
            .and_then(|flags| flags.iter().find(|flag| flag["name"] == name))
            .unwrap_or_else(|| panic!("no flag {name}: {help}"))
    }

    fn names(list: &Value) -> Vec<&str> {
        let mut names: Vec<&str> = list
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn a_command_describes_its_positionals_and_its_flags() {
        let help = help(&["tool", "rescan", "a.rs", "--help-json"]).expect("asked");
        assert_eq!(help["format"], 1);
        assert_eq!(help["command"], "tool rescan");
        assert_eq!(help["path"], json!(["rescan"]));
        assert_eq!(help["about"], "Re-index.");
        assert_eq!(help["aliases"], json!(["rs"]));
        assert_eq!(help["subcommands"], json!([]));
        assert_eq!(help["positional_count"], json!({ "min": 0, "max": null }));
        assert_eq!(
            help["positionals"],
            json!([{
                "name": "paths",
                "value_name": "PATH",
                "required": false,
                "min_values": 0,
                "max_values": null,
                "default": [],
                "possible_values": [],
                "help": "Paths.",
            }])
        );
        assert_eq!(names(&help["flags"]), ["help", "help_json", "limit", "root", "verbose"]);
        assert_eq!(
            flag_named(&help, "limit"),
            &json!({
                "name": "limit",
                "long": "--limit",
                "short": "-n",
                "aliases": ["--max"],
                "takes_value": true,
                "value_name": "LIMIT",
                "required": false,
                "repeatable": false,
                "global": false,
                "default": ["10"],
                "possible_values": ["5", "10"],
                "env": null,
                "help": null,
            })
        );
        assert_eq!(
            flag_named(&help, "verbose"),
            &json!({
                "name": "verbose",
                "long": "--verbose",
                "short": "-v",
                "aliases": [],
                "takes_value": false,
                "value_name": null,
                "required": false,
                "repeatable": false,
                "global": true,
                "default": [],
                "possible_values": [],
                "env": null,
                "help": "Say more.",
            })
        );
        assert_eq!(flag_named(&help, "root")["takes_value"], true);
    }

    #[test]
    fn a_command_with_no_positionals_takes_none() {
        let help = help(&["tool", "scan", "--help-json"]).expect("asked");
        assert_eq!(help["positional_count"], json!({ "min": 0, "max": 0 }));
        assert_eq!(help["positionals"], json!([]));
        assert_eq!(
            names(&help["flags"]),
            ["help", "help_json", "root", "staged", "verbose"]
        );
    }

    #[test]
    fn the_root_lists_its_subcommands() {
        let help = help(&["tool", "--help-json"]).expect("asked");
        assert_eq!(help["command"], "tool");
        assert_eq!(help["path"], json!([]));
        assert_eq!(names(&help["subcommands"]), ["help", "rescan", "scan"]);
        let rescan = help["subcommands"]
            .as_array()
            .and_then(|subs| subs.iter().find(|sub| sub["name"] == "rescan"))
            .expect("rescan");
        assert_eq!(
            rescan,
            &json!({ "name": "rescan", "aliases": ["rs"], "about": "Re-index." })
        );
    }

    #[test]
    fn a_flag_value_is_not_read_as_a_command() {
        let path = |words: &[&str]| help(words).expect("asked")["path"].clone();
        assert_eq!(
            path(&["tool", "--root", "scan", "rescan", "--help-json"]),
            json!(["rescan"])
        );
        assert_eq!(path(&["tool", "--root=rescan", "scan", "--help-json"]), json!(["scan"]));
        assert_eq!(path(&["tool", "rs", "-n", "5", "--help-json"]), json!(["rescan"]));
        assert_eq!(path(&["tool", "-v", "scan", "--help-json"]), json!(["scan"]));
    }

    #[test]
    fn only_a_flag_before_the_separator_asks() {
        assert_eq!(help(&["tool", "scan"]), None);
        assert_eq!(help(&["tool", "rescan", "--", "--help-json"]), None);
    }
}
