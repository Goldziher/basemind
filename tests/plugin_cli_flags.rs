//! Every CLI flag and positional argument the shipped skills and commands give `basemind` is one
//! the command's `--help` lists.
//!
//! The CLI refuses an unknown flag or an extra positional (exit 2), so an example naming one fails
//! as soon as an agent follows it. The skew gate (`tests/plugin_tool_skew.rs`) checks that the
//! commands exist; this file checks what an invocation passes them, and that a flag named on its
//! own in prose belongs to some command.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// The shipped plugin directories whose Markdown gives CLI examples. Each must yield a file.
const CONTENT: [&str; 2] = ["plugin/skills", "plugin/commands"];

/// Flags the plugin text names that belong to another program, with that program. An entry no
/// file names any more fails the test.
const OTHER_PROGRAMS_FLAGS: [(&str, &str); 3] = [
    ("--explain", "the `/bm-stats` slash command"),
    ("--features", "cargo"),
    ("--release", "cargo"),
];

/// Tokens that end the `basemind` part of a shell line.
const SHELL_BREAKS: [&str; 6] = ["|", ">", "&&", "||", ";", "2>&1"];

/// What one command's `--help` says it takes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Help {
    /// Each long flag, and whether it takes a value.
    flags: BTreeMap<String, bool>,
    subcommands: BTreeSet<String>,
    /// How many positional arguments it takes; `None` when the last one repeats.
    positionals: Option<usize>,
}

/// Read a clap `--help` text.
fn parse_help(text: &str) -> Help {
    let option = Regex::new(r"^\s{2,6}(?:-[A-Za-z0-9], )?--([A-Za-z0-9][A-Za-z0-9-]*)(.*)$").expect("option pattern");
    let mut help = Help {
        positionals: Some(0),
        ..Help::default()
    };
    let mut in_commands = false;
    for line in text.lines() {
        if let Some(usage) = line.strip_prefix("Usage: ") {
            let mut count = 0;
            for word in usage.split_whitespace() {
                if !word.starts_with(['<', '[']) || matches!(word, "[OPTIONS]" | "<COMMAND>" | "[COMMAND]") {
                    continue;
                }
                if word.ends_with("...") {
                    help.positionals = None;
                    break;
                }
                count += 1;
                help.positionals = Some(count);
            }
            continue;
        }
        if line.trim_end() == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands {
            match line.strip_prefix("  ").and_then(|rest| rest.split_whitespace().next()) {
                Some(name) if !line.starts_with("   ") => {
                    if name != "help" {
                        help.subcommands.insert(name.to_string());
                    }
                    continue;
                }
                Some(_) => continue,
                None => in_commands = false,
            }
        }
        if let Some(caps) = option.captures(line) {
            let rest = &caps[2];
            let takes_value = rest.starts_with(" <") || rest.starts_with("=<") || rest.starts_with(" [<");
            help.flags.insert(caps[1].to_string(), takes_value);
        }
    }
    help
}

/// The `basemind` invocations in `rest`: for each, its words and quoted strings up to a shell break
/// or a comment, with Markdown's brackets, ellipses and table pipes taken off. A command after
/// `&&`, `||` or `;` counts when it is `basemind` (or the plugin launcher) again. A quoted word is
/// never a shell break, a comment or a bracketed placeholder.
fn tokens(rest: &str) -> Vec<Vec<String>> {
    let rest = rest.replace("\\|", " | ");
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut word: Option<(String, bool)> = None;
    let mut quote = None;
    for ch in rest.chars().chain(std::iter::once(' ')) {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => word.get_or_insert_with(Default::default).0.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                word.get_or_insert_with(Default::default).1 = true;
            }
            None if ch.is_whitespace() => words.extend(word.take()),
            None => word.get_or_insert_with(Default::default).0.push(ch),
        }
    }
    let mut commands: Vec<Vec<String>> = vec![Vec::new()];
    for (word, quoted) in words {
        if !quoted && word.starts_with('#') {
            break;
        }
        if !quoted && SHELL_BREAKS.contains(&word.as_str()) {
            if ["&&", "||", ";"].contains(&word.as_str()) {
                commands.push(Vec::new());
                continue;
            }
            break;
        }
        let word = if quoted {
            word.as_str()
        } else {
            word.trim_start_matches(['[', '('])
                .trim_end_matches([']', ')', ','])
                .trim_end_matches("...")
                .trim_end_matches('…')
                .trim_end_matches(']')
        };
        if !word.is_empty() {
            commands.last_mut().expect("a command").push(word.to_string());
        }
    }
    let mut out = vec![commands.remove(0)];
    for mut command in commands {
        if command
            .first()
            .is_some_and(|name| name == "basemind" || name.ends_with("mcp-launch.sh"))
        {
            command.remove(0);
            out.push(command);
        }
    }
    out
}

/// The name of the long flag `flag` (what follows `--`), and whether it carries its value inline
/// (`--rev=HEAD`).
fn flag_name(flag: &str) -> (&str, bool) {
    flag.split_once('=').map_or((flag, false), |(name, _)| (name, true))
}

/// Why the invocation `rest` (what follows `basemind`) does not run, or nothing when it does or
/// names a command `help` does not know (the skew gate reports those). A flag a command group
/// declares (`basemind --json scan`) is consumed with its value before the subcommand after it is
/// resolved. An unknown flag is taken to carry a value, so its value is not also reported as a
/// positional.
fn invocation_mismatches(rest: &str, help: &dyn Fn(&[String]) -> Option<Help>) -> Vec<String> {
    tokens(rest)
        .into_iter()
        .flat_map(|command| command_mismatches(command, help))
        .collect()
}

/// `invocation_mismatches` for one command's words.
fn command_mismatches(command: Vec<String>, help: &dyn Fn(&[String]) -> Option<Help>) -> Vec<String> {
    let mut words = command.into_iter().peekable();
    let mut path: Vec<String> = Vec::new();
    let Some(mut command) = help(path.as_slice()) else {
        return Vec::new();
    };
    while !command.subcommands.is_empty() {
        let Some(next) = words.peek() else { return Vec::new() };
        if let Some(flag) = next.strip_prefix("--") {
            let (name, inline_value) = flag_name(flag);
            let Some(&takes_value) = command.flags.get(name) else {
                break;
            };
            words.next();
            if takes_value && !inline_value {
                let _ = words.next_if(|value| !value.starts_with('-'));
            }
            continue;
        }
        if next.starts_with('-') {
            break;
        }
        let mut candidate = path.clone();
        candidate.push(next.clone());
        let Some(sub) = help(candidate.as_slice()) else {
            return Vec::new();
        };
        words.next();
        path = candidate;
        command = sub;
    }
    let shown = path.join(" ");
    let mut problems = Vec::new();
    let mut positionals = 0;
    while let Some(word) = words.next() {
        if let Some(flag) = word.strip_prefix("--") {
            let (name, inline_value) = flag_name(flag);
            let takes_value = match command.flags.get(name) {
                None => {
                    problems.push(format!("`basemind {shown}` has no `--{name}`"));
                    true
                }
                Some(takes_value) => *takes_value,
            };
            if takes_value && !inline_value {
                let _ = words.next_if(|value| !value.starts_with('-'));
            }
        } else if !word.starts_with('-') {
            positionals += 1;
        }
    }
    if command.positionals.is_some_and(|most| positionals > most) {
        problems.push(format!(
            "`basemind {shown}` takes {} positional argument(s), the example passes {positionals}",
            command.positionals.unwrap_or_default()
        ));
    }
    problems
}

/// The code in a Markdown text: its inline spans outside fenced blocks, and every line inside one.
fn code_segments(text: &str) -> Vec<String> {
    let span = Regex::new(r"`([^`\n]+)`").expect("span pattern");
    let mut segments = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            segments.push(line.to_string());
        } else {
            segments.extend(span.captures_iter(line).map(|caps| caps[1].to_string()));
        }
    }
    segments
}

/// What follows `basemind` (or the plugin launcher, which runs it) in a code segment.
fn invocation(segment: &str) -> Option<String> {
    let start = Regex::new(r"(?:^|[\s(])(?:basemind|\S*mcp-launch\.sh)\s+(.*)").expect("invocation pattern");
    start.captures(segment).map(|caps| caps[1].to_string())
}

/// Every `--flag` in a code segment; a trailing `*` (`--documents-*`) stands for a family.
fn flags_in(segment: &str) -> Vec<String> {
    let flag = Regex::new(r"(?:^|[^A-Za-z0-9_-])(--[a-z][a-z0-9-]*\*?)").expect("flag pattern");
    flag.captures_iter(segment).map(|caps| caps[1].to_string()).collect()
}

/// `--help` of `basemind <path>`, run in a throwaway home and data directory, cached per path.
struct Cli {
    tmp: tempfile::TempDir,
    seen: RefCell<BTreeMap<Vec<String>, Option<Help>>>,
}

impl Cli {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().expect("tempdir"),
            seen: RefCell::new(BTreeMap::new()),
        }
    }

    fn help(&self, path: &[String]) -> Option<Help> {
        if let Some(known) = self.seen.borrow().get(path) {
            return known.clone();
        }
        let home = self.tmp.path().join("home");
        let output = Command::new(BIN)
            .args(path)
            .arg("--help")
            .current_dir(self.tmp.path())
            .env("BASEMIND_DATA_HOME", self.tmp.path().join("data"))
            .env("BASEMIND_COMMS_DIR", self.tmp.path().join("comms"))
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("NO_COLOR", "1")
            .output()
            .expect("run basemind --help");
        let help = output
            .status
            .success()
            .then(|| parse_help(&String::from_utf8_lossy(&output.stdout)));
        self.seen.borrow_mut().insert(path.to_vec(), help.clone());
        help
    }

    /// Every long flag of every command.
    fn every_flag(&self) -> BTreeSet<String> {
        let mut flags = BTreeSet::new();
        let mut pending = vec![Vec::<String>::new()];
        while let Some(path) = pending.pop() {
            let help = self
                .help(&path)
                .unwrap_or_else(|| panic!("`basemind {} --help` fails", path.join(" ")));
            flags.extend(help.flags.keys().map(|name| format!("--{name}")));
            for sub in help.subcommands {
                let mut child = path.clone();
                child.push(sub);
                pending.push(child);
            }
        }
        flags
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The code segments of every shipped skill and command, with the file each is in.
fn shipped_segments() -> Vec<(String, String)> {
    let root = repo_root();
    let mut segments = Vec::new();
    for dir in CONTENT {
        let mut files = Vec::new();
        markdown_under(&root.join(dir), &mut files);
        assert!(!files.is_empty(), "{dir} holds no Markdown file");
        for file in files {
            let text =
                std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
            let shown = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
            segments.extend(code_segments(&text).into_iter().map(|segment| (shown.clone(), segment)));
        }
    }
    segments
}

fn markdown_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_under(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

#[test]
fn every_flag_and_positional_an_example_passes_is_one_the_command_takes() {
    let cli = Cli::new();
    let mut checked = 0;
    let mut problems = Vec::new();
    for (file, segment) in shipped_segments() {
        let Some(rest) = invocation(&segment) else { continue };
        checked += 1;
        problems.extend(
            invocation_mismatches(&rest, &|path| cli.help(path))
                .into_iter()
                .map(|problem| format!("{file}: `{segment}`: {problem}")),
        );
    }
    assert!(
        checked >= 50,
        "only {checked} `basemind` invocations found: the extraction is broken"
    );
    assert!(
        problems.is_empty(),
        "the plugin passes `basemind` what its commands do not take:\n{}",
        problems.join("\n")
    );
}

#[test]
fn every_flag_named_in_a_skill_or_command_belongs_to_a_command() {
    let known = Cli::new().every_flag();
    assert!(
        known.contains("--json"),
        "the flag walk found no global flag: {known:?}"
    );
    let mut unknown = Vec::new();
    let mut other_used = BTreeSet::new();
    for (file, segment) in shipped_segments() {
        for flag in flags_in(&segment) {
            if let Some((other, _)) = OTHER_PROGRAMS_FLAGS.iter().find(|(other, _)| *other == flag) {
                other_used.insert(*other);
                continue;
            }
            let served = match flag.strip_suffix('*') {
                Some(family) => known.iter().any(|known| known.starts_with(family)),
                None => known.contains(&flag),
            };
            if !served {
                unknown.push(format!("{file}: `{flag}` in `{segment}`"));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "the plugin names flags no `basemind` command has:\n{}",
        unknown.join("\n")
    );
    let stale: Vec<&str> = OTHER_PROGRAMS_FLAGS
        .iter()
        .map(|(flag, _)| *flag)
        .filter(|flag| !other_used.contains(flag))
        .collect();
    assert!(
        stale.is_empty(),
        "OTHER_PROGRAMS_FLAGS entries no file names any more: {stale:?}"
    );
}

/// The `--help` texts of a small CLI: `scan`, `rescan` and the `code` group with `grep`.
fn fixture_help(path: &[String]) -> Option<Help> {
    let text = match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            "Usage: basemind [OPTIONS] <COMMAND>\n\nCommands:\n  scan    Scan\n  rescan  Re-index\n  code    \
               Read the code map\n  help    Print this message\n\nOptions:\n      --json\n          JSON output\n"
        }
        ["scan"] => {
            "Scan\n\nUsage: basemind scan [OPTIONS]\n\nOptions:\n      --staged\n          Index the staging \
                     area\n\n      --rev <REV>\n          Index the tree at REV\n\n  -h, --help\n          Print help\n"
        }
        ["rescan"] => "Usage: basemind rescan [OPTIONS] [PATH]...\n\nArguments:\n  [PATH]...\n          Paths\n",
        ["code"] => {
            "Usage: basemind code <COMMAND>\n\nCommands:\n  grep  Exact text search\n  help  Print this \
                     message\n\nOptions:\n  -h, --help  Print help\n"
        }
        ["code", "grep"] => {
            "Usage: basemind code grep [OPTIONS] <PATTERN>\n\nOptions:\n      --regex\n          \
                             Regex\n      --path <PATH>\n          Path filter\n"
        }
        _ => return None,
    };
    Some(parse_help(text))
}

#[test]
fn clap_help_is_read_as_flags_subcommands_and_positionals() {
    let flags = |pairs: &[(&str, bool)]| {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), *value))
            .collect()
    };
    assert_eq!(
        fixture_help(&["scan".to_string()]),
        Some(Help {
            flags: flags(&[("help", false), ("rev", true), ("staged", false)]),
            subcommands: BTreeSet::new(),
            positionals: Some(0),
        })
    );
    assert_eq!(
        fixture_help(&["rescan".to_string()]).map(|help| help.positionals),
        Some(None)
    );
    assert_eq!(
        fixture_help(&["code".to_string()]).map(|help| help.subcommands),
        Some(BTreeSet::from(["grep".to_string()]))
    );
    assert_eq!(
        fixture_help(&["code".to_string(), "grep".to_string()]).map(|help| (help.flags, help.positionals)),
        Some((flags(&[("path", true), ("regex", false)]), Some(1)))
    );
}

#[test]
fn an_unknown_flag_or_an_extra_positional_is_reported() {
    for (rest, expected) in [
        (
            "scan <path>     # scope to a path",
            vec!["`basemind scan` takes 0 positional argument(s), the example passes 1"],
        ),
        (
            "code grep \"pattern\" [--regex --path P --scope S]",
            vec!["`basemind code grep` has no `--scope`"],
        ),
        ("scan --rev HEAD~3 --staged", vec![]),
        ("--json scan --staged", vec![]),
        ("--json code grep x --regex", vec![]),
        (
            "--json scan <path>",
            vec!["`basemind scan` takes 0 positional argument(s), the example passes 1"],
        ),
        ("rescan src/a.rs src/b.rs", vec![]),
        ("code grep 'fn \\w+_call' --regex --path src/ | head", vec![]),
        ("code stats|gc", vec![]),
        ("<command> --help", vec![]),
        (
            "code grep '#' extra",
            vec!["`basemind code grep` takes 1 positional argument(s), the example passes 2"],
        ),
        ("code grep '#'", vec![]),
        (
            "scan && basemind scan src/lib.rs",
            vec!["`basemind scan` takes 0 positional argument(s), the example passes 1"],
        ),
        ("scan && basemind scan --staged", vec![]),
    ] {
        assert_eq!(invocation_mismatches(rest, &fixture_help), expected, "{rest}");
    }
}

#[test]
fn invocations_and_flags_are_read_from_code_only() {
    let text = "Run `basemind scan` or `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh rescan <path>`; basemind \
                scan --prose is prose.\n\n```sh\nbasemind code grep x --path src/   # a comment\n```\n";
    let segments = code_segments(text);
    assert_eq!(
        segments
            .iter()
            .filter_map(|segment| invocation(segment))
            .collect::<Vec<_>>(),
        vec![
            "scan".to_string(),
            "rescan <path>".to_string(),
            "code grep x --path src/   # a comment".to_string()
        ]
    );
    assert_eq!(
        flags_in("a `--documents-*` / `--llm-*` override, not x--y"),
        vec!["--documents-*", "--llm-*"]
    );
}
