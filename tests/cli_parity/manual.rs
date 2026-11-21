//! The offline manual (`docs/manual/*.md`, issue #133) against the surface it describes: every
//! `basemind …` command it names is a real command with real flags, every tool it names is
//! advertised and takes the arguments shown, every `BASEMIND_*` variable is read somewhere in
//! `src/`, and every tool and top-level command is covered by some page.

use std::collections::BTreeSet;
use std::path::Path;
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

/// A token that stands for something the reader fills in: stop checking there.
fn placeholder(token: &str) -> bool {
    token.starts_with('<') || token.starts_with('[') || token.contains('…')
}

/// Check one `basemind …` mention against the live tree; `Err` says what is wrong.
fn check_command(tree: &cli_tree::CliTree, mention: &str) -> Result<(), String> {
    let mut tokens = mention.split_whitespace().skip(1).peekable();
    if let Some(first) = tokens.peek()
        && COMMS_COMMANDS.contains(first)
        && !comms_build()
    {
        return Ok(());
    }
    let mut path = String::new();
    for token in tokens {
        if placeholder(token) {
            return Ok(());
        }
        let node = tree.commands.get(&path).ok_or_else(|| format!("no command `{path}`"))?;
        if !node.is_leaf() {
            if token == "--help" || token == "-h" {
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

fn source_texts(dir: &Path, out: &mut String) {
    for entry in std::fs::read_dir(dir).expect("read src") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            source_texts(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push_str(&std::fs::read_to_string(&path).expect("read source"));
        }
    }
}

/// The `BASEMIND_*` variable names `text` mentions. A name ending in `_` is a family written as a
/// wildcard (`BASEMIND_DOCUMENTS_*`), not a variable, and is skipped.
fn variable_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find("BASEMIND_") {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..end];
        if name.len() > "BASEMIND_".len() && !name.ends_with('_') {
            names.insert(name.to_string());
        }
        rest = &tail[end..];
    }
    names
}

#[test]
fn variable_names_skips_a_wildcard_family() {
    let names =
        variable_names("Set `BASEMIND_DATA_HOME`, any `BASEMIND_DOCUMENTS_*` switch, `BASEMIND_` or BASEMIND_LLM_2.");
    assert_eq!(
        names.into_iter().collect::<Vec<_>>(),
        ["BASEMIND_DATA_HOME", "BASEMIND_LLM_2"]
    );
}

#[test]
fn every_environment_variable_the_manual_names_is_read_by_the_source() {
    let mut source = String::new();
    source_texts(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut source);
    let names: BTreeSet<String> = pages().into_iter().flat_map(|(_, text)| variable_names(text)).collect();
    let unknown: Vec<&String> = names
        .iter()
        .filter(|name| !source.contains(&format!("\"{name}\"")))
        .collect();
    assert!(
        names.len() >= 15,
        "only {} variables found; the extraction broke",
        names.len()
    );
    assert!(
        unknown.is_empty(),
        "the manual names variables no source reads: {unknown:?}"
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
