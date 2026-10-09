//! The real `basemind` command tree, recovered from the built binary's own `-h` output.
//!
//! The top-level `Cmd` enum lives in the binary crate (`src/main.rs`), so an integration test cannot
//! call `CommandFactory` on it. Clap's compact help (`-h`) is a stable, machine-regular rendering of
//! exactly the same tree (`Commands:` / `Arguments:` / `Options:` sections, `[default: …]` and
//! `[possible values: …]` suffixes, required flags spelled out in the `Usage:` line), so the walk
//! below reads the binary the user actually runs and cannot drift from it. Hidden commands and
//! flags are, by construction, not part of the public surface and are not walked.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// One argument of a CLI command, positional or flag.
#[derive(Debug, Clone)]
pub struct CliArg {
    /// Lower snake_case identifier: the long flag name or the positional's value name.
    pub id: String,
    /// `--kebab-name` for a flag, `<NAME>` for a positional (used in diagnostics).
    pub display: String,
    pub positional: bool,
    /// A flag that takes no value (a boolean switch).
    pub switch: bool,
    /// Repeatable / multi-value (`<PATH>...`).
    pub variadic: bool,
    pub required: bool,
    pub possible_values: Vec<String>,
    pub default: Option<String>,
}

/// One node of the command tree.
#[derive(Debug, Clone)]
pub struct CliCommand {
    /// Space-joined path without the `basemind` prefix; `""` for the root.
    pub path: String,
    pub subcommands: Vec<String>,
    /// Arguments excluding the global flags and `-h/--help`.
    pub args: Vec<CliArg>,
}

impl CliCommand {
    pub fn is_leaf(&self) -> bool {
        self.subcommands.is_empty()
    }
}

pub struct CliTree {
    pub commands: BTreeMap<String, CliCommand>,
    /// Ids of the global flags (`--root`, `--json`, …) declared once on the root.
    pub globals: BTreeSet<String>,
}

impl CliTree {
    pub fn leaves(&self) -> impl Iterator<Item = &CliCommand> {
        self.commands.values().filter(|c| !c.path.is_empty() && c.is_leaf())
    }
}

fn help_text(path: &str) -> String {
    let mut args: Vec<&str> = path.split(' ').filter(|s| !s.is_empty()).collect();
    args.push("-h");
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(&args)
        .output()
        .unwrap_or_else(|e| panic!("spawn `basemind {path} -h`: {e}"));
    assert!(
        output.status.success(),
        "`basemind {path} -h` exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn snake(raw: &str) -> String {
    raw.trim_matches(|c| matches!(c, '<' | '>' | '[' | ']' | '.' | ','))
        .to_lowercase()
        .replace('-', "_")
}

/// The text of a `[key: …]` suffix in a help description, if present.
fn bracketed(desc: &str, key: &str) -> Option<String> {
    let start = desc.find(&format!("[{key}: "))? + key.len() + 3;
    let rest = &desc[start..];
    Some(rest[..rest.find(']')?].to_string())
}

fn split_columns(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    match line.find("  ") {
        Some(i) => (&line[..i], line[i..].trim()),
        None => (line, ""),
    }
}

/// Parse one command's `-h` text. `globals` filters the flags every command inherits.
fn parse_help(path: &str, text: &str, globals: &BTreeSet<String>) -> CliCommand {
    let required_flags: BTreeSet<String> = text
        .lines()
        .find(|l| l.starts_with("Usage:"))
        .map(|usage| {
            usage
                .split_whitespace()
                .filter_map(|tok| tok.strip_prefix("--"))
                .map(snake)
                .collect()
        })
        .unwrap_or_default();

    // Gather `(spec, description)` entries per section. Clap switches to next-line help when a
    // description is long, putting it on following lines indented 8+ spaces; fold those back in.
    let mut entries: Vec<(&str, String, String)> = Vec::new();
    let mut section = "";
    for line in text.lines() {
        match line.trim_end() {
            "Commands:" => section = "commands",
            "Arguments:" => section = "arguments",
            "Options:" => section = "options",
            "" => section = "",
            _ => {}
        }
        let indent = line.len() - line.trim_start().len();
        if section.is_empty() || line.trim_end().ends_with(':') && indent == 0 {
            continue;
        }
        if indent >= 8 {
            if let Some((_, _, desc)) = entries.last_mut() {
                desc.push(' ');
                desc.push_str(line.trim());
            }
        } else if indent >= 2 {
            let (spec, desc) = split_columns(line);
            entries.push((section, spec.to_string(), desc.to_string()));
        }
    }

    let (mut subcommands, mut args) = (Vec::new(), Vec::new());
    for (section, spec, desc) in &entries {
        let (spec, desc) = (spec.as_str(), desc.as_str());
        match *section {
            "commands" => {
                let name = spec.split_whitespace().next().unwrap_or_default();
                if name != "help" {
                    subcommands.push(name.to_string());
                }
            }
            "arguments" => {
                let id = snake(spec);
                args.push(CliArg {
                    display: spec.trim_end_matches("...").to_string(),
                    required: spec.starts_with('<'),
                    variadic: spec.ends_with("..."),
                    positional: true,
                    switch: false,
                    possible_values: split_list(bracketed(desc, "possible values")),
                    default: bracketed(desc, "default"),
                    id,
                });
            }
            "options" => {
                let Some(long) = spec
                    .split_whitespace()
                    .find_map(|t| t.trim_end_matches(',').strip_prefix("--"))
                else {
                    continue;
                };
                let id = snake(long);
                if id == "help" || id == "version" || globals.contains(&id) {
                    continue;
                }
                let value = spec
                    .split_whitespace()
                    .find(|t| t.starts_with('<') || t.starts_with("[<"));
                args.push(CliArg {
                    display: format!("--{long}"),
                    positional: false,
                    switch: value.is_none(),
                    variadic: value.is_some_and(|v| v.ends_with("...")),
                    required: required_flags.contains(&id),
                    possible_values: split_list(bracketed(desc, "possible values")),
                    default: bracketed(desc, "default"),
                    id,
                });
            }
            _ => {}
        }
    }
    CliCommand {
        path: path.to_string(),
        subcommands,
        args,
    }
}

fn split_list(raw: Option<String>) -> Vec<String> {
    raw.map(|s| s.split(", ").map(str::to_string).collect())
        .unwrap_or_default()
}

/// Walk the whole tree by invoking `basemind [path…] -h` recursively.
fn walk() -> CliTree {
    let root_text = help_text("");
    let globals: BTreeSet<String> = {
        let root = parse_help("", &root_text, &BTreeSet::new());
        root.args
            .iter()
            .filter(|a| !a.positional)
            .map(|a| a.id.clone())
            .collect()
    };
    let mut commands = BTreeMap::new();
    let mut queue = vec![String::new()];
    while let Some(path) = queue.pop() {
        let text = if path.is_empty() {
            root_text.clone()
        } else {
            help_text(&path)
        };
        let command = parse_help(&path, &text, &globals);
        for sub in &command.subcommands {
            queue.push(if path.is_empty() {
                sub.clone()
            } else {
                format!("{path} {sub}")
            });
        }
        commands.insert(path, command);
    }
    CliTree { commands, globals }
}

/// The command tree, walked once per test process.
pub fn load() -> &'static CliTree {
    static TREE: std::sync::OnceLock<CliTree> = std::sync::OnceLock::new();
    TREE.get_or_init(walk)
}
