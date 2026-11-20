//! The offline manual (`docs/manual/*.md`, issue #133) against the surface it describes: every
//! `basemind …` command it names is a real command with real flags, every tool it names is
//! advertised and takes the arguments shown, every `BASEMIND_*` variable it names is read by the
//! shipped code and every one that code reads is in the environment table, and every tool and
//! top-level command is covered by some page.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::cli::manual::{INDEX, PAGES};
use serde_json::Value;

use crate::{cli_tree, mcp_surface};

/// The tool-name families of the MCP surface; a code span `<family>_<rest>` names a tool.
const TOOL_FAMILIES: &[&str] = &["code", "git", "docs", "memory", "index", "find", "agent"];
/// Top-level commands that exist only in a `comms` build.
const COMMS_COMMANDS: &[&str] = &["comms", "agent", "workspace", "daemon"];

/// `(page, text)` for the index and every topic.
fn pages() -> Vec<(&'static str, &'static str)> {
    std::iter::once(("index", INDEX))
        .chain(PAGES.iter().map(|page| (page.topic, page.text)))
        .collect()
}

/// Inline code spans outside fences, plus every fenced line, of one page.
fn code_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            spans.push(line.trim().to_string());
        } else {
            spans.extend(line.split('`').skip(1).step_by(2).map(str::to_string));
        }
    }
    spans
}

fn comms_build() -> bool {
    cfg!(all(feature = "comms", any(unix, windows)))
}

/// A token that stands for something the reader fills in. In a subcommand's place it hides the
/// rest of the path, so checking stops; in a leaf's arguments it is a value, so checking goes on.
fn placeholder(token: &str) -> bool {
    token.starts_with('<') || token.starts_with('[') || token.contains('…')
}

/// Check one `basemind …` mention against the live tree; `Err` says what is wrong.
pub(crate) fn check_command(tree: &cli_tree::CliTree, mention: &str) -> Result<(), String> {
    let mut tokens = mention.split_whitespace().skip(1).peekable();
    if let Some(first) = tokens.peek()
        && COMMS_COMMANDS.contains(first)
        && !comms_build()
    {
        return Ok(());
    }
    let mut path = String::new();
    for token in tokens {
        let node = tree.commands.get(&path).ok_or_else(|| format!("no command `{path}`"))?;
        if !node.is_leaf() {
            if placeholder(token) || token == "--help" || token == "-h" {
                return Ok(());
            }
            if !node.subcommands.iter().any(|sub| sub == token) {
                return Err(format!("`basemind {path}` has no subcommand `{token}`"));
            }
            path = if path.is_empty() {
                token.to_string()
            } else {
                format!("{path} {token}")
            };
            continue;
        }
        if placeholder(token) {
            continue;
        }
        if let Some(flag) = token.strip_prefix("--") {
            let id = flag.split('=').next().unwrap_or(flag).replace('-', "_");
            if id != "help" && !node.args.iter().any(|arg| arg.id == id) && !tree.globals.contains(&id) {
                return Err(format!("`basemind {path}` has no flag `--{flag}`"));
            }
        }
    }
    Ok(())
}

#[test]
fn every_command_the_manual_names_exists_with_its_flags() {
    let tree = cli_tree::load();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (page, text) in pages() {
        for span in code_spans(text) {
            if !span.starts_with("basemind ") {
                continue;
            }
            checked += 1;
            if let Err(why) = check_command(tree, &span) {
                failures.push(format!("{page}.md: `{span}`: {why}"));
            }
        }
    }
    assert!(
        checked >= 80,
        "only {checked} command mentions found; the extraction broke"
    );
    assert!(
        failures.is_empty(),
        "the manual names commands the CLI lacks:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn a_mistyped_command_or_flag_is_caught() {
    let tree = cli_tree::load();
    assert!(check_command(tree, "basemind code outline src/a.rs").is_ok());
    assert!(check_command(tree, "basemind code outlines src/a.rs").is_err());
    assert!(check_command(tree, "basemind git log --paths src/").is_err());
    assert!(check_command(tree, "basemind git log --path src/ --json").is_ok());
    assert!(check_command(tree, "basemind git log --path <path> --jsno").is_err());
    assert!(check_command(tree, "basemind code read <path> --line <n> --end-line <m>").is_ok());
    assert!(check_command(tree, "basemind <command> --help").is_ok());
}

/// `(name, arguments)` of a span naming a tool: `code_refs` or `code_refs {"name":"x"}`.
fn tool_mention(span: &str) -> Option<(&str, Option<&str>)> {
    let (name, rest) = span.split_once(' ').map_or((span, None), |(n, r)| (n, Some(r)));
    let (family, tail) = name.split_once('_')?;
    let shaped = TOOL_FAMILIES.contains(&family)
        && !tail.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    shaped.then_some((name, rest.filter(|r| r.starts_with('{'))))
}

#[test]
fn every_tool_the_manual_names_is_advertised_with_those_arguments() {
    let domains = mcp_surface::domains();
    let mut failures = Vec::new();
    let mut mentioned = BTreeSet::new();
    for (page, text) in pages() {
        for span in code_spans(text) {
            let Some((name, arguments)) = tool_mention(&span) else {
                continue;
            };
            mentioned.insert(name.to_string());
            if name.starts_with("agent_") && !comms_build() {
                continue;
            }
            let Some(domain) = domains.get(name) else {
                failures.push(format!("{page}.md: `{name}` is not an advertised tool"));
                continue;
            };
            let Some(arguments) = arguments else { continue };
            match serde_json::from_str::<Value>(arguments) {
                Ok(Value::Object(fields)) => {
                    for field in fields.keys().filter(|field| !domain.fields.contains_key(*field)) {
                        failures.push(format!("{page}.md: `{name}` takes no `{field}`"));
                    }
                }
                _ => failures.push(format!("{page}.md: `{span}` arguments are not a JSON object")),
            }
        }
    }
    for tool in domains.keys().filter(|tool| !mentioned.contains(*tool)) {
        failures.push(format!("no manual page mentions the tool `{tool}`"));
    }
    assert!(
        mentioned.len() >= 15,
        "only {} tools mentioned; the extraction broke",
        mentioned.len()
    );
    assert!(
        failures.is_empty(),
        "manual and MCP surface disagree:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn every_top_level_command_has_a_manual_mention() {
    let tree = cli_tree::load();
    let root = tree.commands.get("").expect("root command");
    let spans: Vec<String> = pages().into_iter().flat_map(|(_, text)| code_spans(text)).collect();
    let missing: Vec<&String> = root
        .subcommands
        .iter()
        .filter(|command| *command != "help")
        .filter(|command| {
            let prefix = format!("basemind {command}");
            !spans
                .iter()
                .any(|span| span == &prefix || span.starts_with(&format!("{prefix} ")))
        })
        .collect();
    assert!(root.subcommands.len() > 15, "the command tree came back short");
    assert!(
        missing.is_empty(),
        "no manual page names `basemind <command>` for: {missing:?}"
    );
}

/// Shipped Rust sources, where a variable is read through a string literal naming it whole.
const RUST_SOURCES: &[&str] = &["src", "crates/basemind-agents/src"];
/// The plugin's script and hook sources and the npm and pip wrappers, where any mention counts.
const SCRIPT_SOURCES: &[&str] = &[
    "plugin/.ai-rulez/scripts",
    "plugin/.ai-rulez/hooks",
    "npm-package/bin",
    "npm-package/index.js",
    "npm-package/install.js",
    "pip-package/basemind",
];
/// Variables only tests set, left out of the table along with every `BASEMIND_TEST_*`.
const TEST_ONLY: &[&str] = &["BASEMIND_ALLOC_TAG_TEST_CHILD", "BASEMIND_AGENTS_SIGNAL_MASK_CHILD"];

/// Every file under `path`, or `path` itself when it is a file.
fn files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    for entry in std::fs::read_dir(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())) {
        let path = entry.expect("entry").path();
        let skipped = path
            .file_name()
            .is_some_and(|name| name == "node_modules" || name == "__pycache__");
        if !skipped {
            files(&path, out);
        }
    }
}

/// Every `BASEMIND_*` name in `text`; with `quoted`, only one that is a whole string literal. A name
/// ending in `_` is a prefix, not a variable.
fn variable_names(text: &str, quoted: bool) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find("BASEMIND_") {
        let opens = rest[..at].ends_with('"');
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..end];
        if !name.ends_with('_') && (!quoted || (opens && tail[end..].starts_with('"'))) {
            names.insert(name.to_string());
        }
        rest = &tail[end..];
    }
    names
}

/// The variables the shipped code reads, outside `*_tests.rs` files.
fn read_variables() -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut names = BTreeSet::new();
    for (sources, quoted) in [(RUST_SOURCES, true), (SCRIPT_SOURCES, false)] {
        let mut paths = Vec::new();
        for source in sources {
            files(&root.join(source), &mut paths);
        }
        for path in paths {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            if quoted && (!name.ends_with(".rs") || name.ends_with("_tests.rs")) {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            names.extend(variable_names(&String::from_utf8_lossy(&bytes), quoted));
        }
    }
    names
}

#[test]
fn a_variable_is_a_whole_name_and_in_rust_a_whole_literal() {
    let text = r#"let a = "BASEMIND_ONE"; "BASEMIND_TWO is set"; "BASEMIND_DOCUMENTS_"; BASEMIND_THREE"#;
    let names = |quoted| variable_names(text, quoted).into_iter().collect::<Vec<_>>();
    assert_eq!(names(true), ["BASEMIND_ONE"]);
    assert_eq!(names(false), ["BASEMIND_ONE", "BASEMIND_THREE", "BASEMIND_TWO"]);
}

#[test]
fn variable_names_skips_a_wildcard_family() {
    let names = variable_names(
        "Set `BASEMIND_DATA_HOME`, any `BASEMIND_DOCUMENTS_*` switch, `BASEMIND_` or BASEMIND_LLM_2.",
        false,
    );
    assert_eq!(
        names.into_iter().collect::<Vec<_>>(),
        ["BASEMIND_DATA_HOME", "BASEMIND_LLM_2"]
    );
}

#[test]
fn every_environment_variable_the_manual_names_is_read_by_the_source() {
    let read = read_variables();
    let names: BTreeSet<String> = pages()
        .into_iter()
        .flat_map(|(_, text)| variable_names(text, false))
        .collect();
    let unknown: Vec<&String> = names.difference(&read).collect();
    assert!(
        names.len() >= 80,
        "only {} variables found; the extraction broke",
        names.len()
    );
    assert!(
        unknown.is_empty(),
        "the manual names variables no source reads: {unknown:?}"
    );
}

#[test]
fn every_variable_the_source_reads_is_in_the_environment_table() {
    let read = read_variables();
    let page = PAGES
        .iter()
        .find(|page| page.topic == "environment")
        .expect("environment page");
    let table = variable_names(page.text, false);
    let missing: Vec<&String> = read
        .difference(&table)
        .filter(|name| !name.starts_with("BASEMIND_TEST_") && !TEST_ONLY.contains(&name.as_str()))
        .collect();
    assert!(
        read.len() >= 80,
        "only {} variables read; the extraction broke",
        read.len()
    );
    assert!(
        missing.is_empty(),
        "docs/manual/environment.md lacks variables the source reads: {missing:?}"
    );
}

fn manual(args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().expect("data home");
    let comms = tempfile::tempdir().expect("comms dir");
    let cwd = tempfile::tempdir().expect("not a project");
    Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("manual")
        .args(args)
        .current_dir(cwd.path())
        .env("BASEMIND_DATA_HOME", home.path())
        .env("BASEMIND_COMMS_DIR", comms.path())
        .output()
        .expect("run basemind manual")
}

#[test]
fn manual_prints_the_index_and_a_page_outside_any_project() {
    let index = manual(&[]);
    assert!(index.status.success(), "{}", String::from_utf8_lossy(&index.stderr));
    assert_eq!(String::from_utf8_lossy(&index.stdout), INDEX);
    let page = manual(&["resolution"]);
    assert!(page.status.success(), "{}", String::from_utf8_lossy(&page.stderr));
    let expected = PAGES.iter().find(|page| page.topic == "resolution").expect("page").text;
    assert_eq!(String::from_utf8_lossy(&page.stdout), expected);
}

#[test]
fn an_unknown_topic_is_a_usage_error_listing_the_topics() {
    let output = manual(&["resolutoin"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolution"),
        "the refusal should suggest the topic:\n{stderr}"
    );
}
