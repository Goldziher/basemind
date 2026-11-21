//! Every argument key the shipped skills, commands and hooks give a verb tool is one that tool's
//! schema has (`tests/fixtures/tools_list.json`).
//!
//! The tools reject an unknown argument (`deny_unknown_fields`), so an example naming one fails as
//! soon as an agent follows it. The skew gate (`tests/plugin_tool_skew.rs`) checks the arguments of
//! call-shaped examples (`code_grep {"pattern":"x"}`) against a running server; this file also checks
//! the keys that stand alone in prose, such as `"regex":true` or `"scope":"web:<host>"`, against the
//! tools the same paragraph (or, outside Markdown, the same line) names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regex::Regex;

/// The committed `tools/list` snapshot of the verb tools.
const TOOLS_LIST: &str = "tests/fixtures/tools_list.json";

/// The shipped plugin directories whose text gives tool examples. Each must yield a file.
const CONTENT: [&str; 3] = ["plugin/skills", "plugin/commands", "plugin/hooks"];

/// Each verb tool's argument names.
type Arguments = BTreeMap<String, BTreeSet<String>>;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tool_arguments() -> Arguments {
    let text = std::fs::read_to_string(repo_root().join(TOOLS_LIST)).expect("read the tools/list snapshot");
    let tools: serde_json::Value = serde_json::from_str(&text).expect("tools/list snapshot is JSON");
    tools
        .as_array()
        .expect("the snapshot is an array of tools")
        .iter()
        .map(|tool| {
            let name = tool["name"].as_str().expect("a tool name").to_string();
            let arguments: BTreeSet<String> = tool["inputSchema"]["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default();
            (name, arguments)
        })
        .collect()
}

/// Every file under `dir`.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Matches the argument checks need, built once per call.
struct Patterns {
    /// A verb tool's name as a whole word.
    tool: Regex,
    /// A verb tool's name opening a call: `code_grep {`.
    call: Regex,
    /// An object key, quoted or escape-quoted: `"path":`, `\"path\":`.
    key: Regex,
    /// A span that is one quoted key alone: `"path"`.
    lone_key: Regex,
    /// An inline code span.
    span: Regex,
    /// A fenced code block.
    fence: Regex,
}

impl Patterns {
    fn new(arguments: &Arguments) -> Self {
        let mut names: Vec<&str> = arguments.keys().map(String::as_str).collect();
        names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        let names = names.join("|");
        Self {
            tool: Regex::new(&format!(r"\b({names})\b")).expect("tool pattern"),
            call: Regex::new(&format!(r"^({names})\s*\{{")).expect("call pattern"),
            key: Regex::new(r#"\\?"([A-Za-z_][A-Za-z0-9_]*)\\?"\s*:"#).expect("key pattern"),
            lone_key: Regex::new(r#"^"([A-Za-z_][A-Za-z0-9_]*)"$"#).expect("lone key pattern"),
            span: Regex::new(r"`([^`\n]+)`").expect("span pattern"),
            fence: Regex::new(r"(?s)```.*?```").expect("fence pattern"),
        }
    }

    /// The arguments of every tool `text` names.
    fn named<'a>(&self, text: &str, arguments: &'a Arguments) -> BTreeSet<&'a String> {
        self.tool
            .find_iter(text)
            .filter_map(|tool| arguments.get(tool.as_str()))
            .flatten()
            .collect()
    }
}

/// The keys in `text` no tool takes, each as `` `<where>`: `<key>` ``.
///
/// In Markdown, outside fenced blocks (which hold answers as well as calls): an inline span that
/// opens a call is checked against that tool, and a span that starts with a key or an object against
/// the tools its paragraph names. In any other file, a line's keys are checked against the tools the
/// line names.
fn unknown_keys(text: &str, markdown: bool, arguments: &Arguments, patterns: &Patterns) -> Vec<String> {
    let mut unknown = Vec::new();
    if !markdown {
        for line in text.lines() {
            let allowed = patterns.named(line, arguments);
            if allowed.is_empty() {
                continue;
            }
            for key in patterns.key.captures_iter(line).map(|caps| caps[1].to_string()) {
                if !allowed.contains(&key) {
                    unknown.push(format!("`{}`: `{key}`", line.trim()));
                }
            }
        }
        return unknown;
    }
    let prose = patterns.fence.replace_all(text, "");
    for paragraph in prose.split("\n\n") {
        let named = patterns.named(paragraph, arguments);
        for span in patterns.span.captures_iter(paragraph).map(|caps| caps[1].to_string()) {
            let allowed: BTreeSet<&String> = match patterns.call.captures(&span) {
                Some(call) => arguments[&call[1]].iter().collect(),
                None if !named.is_empty() && span.trim_start().starts_with(['{', '"']) => named.clone(),
                None => continue,
            };
            let mut keys: Vec<String> = patterns
                .key
                .captures_iter(&span)
                .map(|caps| caps[1].to_string())
                .collect();
            if keys.is_empty() {
                keys.extend(patterns.lone_key.captures(&span).map(|caps| caps[1].to_string()));
            }
            for key in keys {
                if !allowed.contains(&key) {
                    unknown.push(format!("`{span}`: `{key}`"));
                }
            }
        }
    }
    unknown
}

#[test]
fn every_argument_key_in_a_skill_command_or_hook_is_one_the_tool_takes() {
    let root = repo_root();
    let arguments = tool_arguments();
    assert!(
        arguments.len() >= 10,
        "{TOOLS_LIST} lists only {} tools",
        arguments.len()
    );
    let patterns = Patterns::new(&arguments);
    let mut unknown = Vec::new();
    for dir in CONTENT {
        let mut files = Vec::new();
        files_under(&root.join(dir), &mut files);
        let mut read = 0;
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            read += 1;
            let markdown = file.extension().is_some_and(|ext| ext == "md");
            let shown = file.strip_prefix(&root).unwrap_or(file).display().to_string();
            unknown.extend(
                unknown_keys(&text, markdown, &arguments, &patterns)
                    .into_iter()
                    .map(|key| format!("{shown}: {key}")),
            );
        }
        assert!(read > 0, "{dir} holds no readable file");
    }
    assert!(
        unknown.is_empty(),
        "the plugin gives tools arguments their schema in {TOOLS_LIST} lacks:\n{}",
        unknown.join("\n")
    );
}

fn fixture_arguments() -> Arguments {
    [
        ("docs_search", &["query", "path"][..]),
        ("code_grep", &["pattern", "regex", "path"][..]),
    ]
    .into_iter()
    .map(|(tool, args)| (tool.to_string(), args.iter().map(|arg| (*arg).to_string()).collect()))
    .collect()
}

#[test]
fn a_key_no_named_tool_takes_is_reported_wherever_it_stands() {
    let arguments = fixture_arguments();
    let patterns = Patterns::new(&arguments);
    let markdown = "- `docs_search` searches this repo by default; pass\n  `\"scope\":\"web:<host>\"` to search \
                    one host.\n\nAdd `\"regex\":true` to `code_grep`, and `\"path\"` to scope it.\n\n\
                    `code_grep {\"pattern\":\"x\",\"query\":\"y\"}`\n";
    assert_eq!(
        unknown_keys(markdown, true, &arguments, &patterns),
        vec![
            "`\"scope\":\"web:<host>\"`: `scope`".to_string(),
            "`code_grep {\"pattern\":\"x\",\"query\":\"y\"}`: `query`".to_string(),
        ]
    );
    let hook = "alt='code_grep {\"pattern\":\"…\"} (add \"regex\":true or \"glob\":\"*.rs\")'\n";
    assert_eq!(
        unknown_keys(hook, false, &arguments, &patterns),
        vec![format!("`{}`: `glob`", hook.trim())]
    );
}

#[test]
fn answers_in_fenced_blocks_and_paragraphs_naming_no_tool_are_not_arguments() {
    let arguments = fixture_arguments();
    let patterns = Patterns::new(&arguments);
    let markdown = "```text\ncode_grep {\"pattern\":\"x\"}\n→ {\"hits\":{\"a.rs\":{\"\":[\"1: x\"]}}}\n```\n\n\
                    A page lists `{\"total\":3}` and `files: {\"<sha>\": {\"<path>\": \"<change>\"}}`.\n";
    assert_eq!(
        unknown_keys(markdown, true, &arguments, &patterns),
        Vec::<String>::new()
    );
}
