//! Plugin skew gate: every MCP tool, mode, argument and CLI command the shipped plugin content names
//! must be served by the binary that content runs against, and every tool must answer one call.
//!
//! The plugin's hooks, skills, commands and rules tell the model which tools to call; the plugin
//! launcher (`scripts/mcp-launch.sh`) starts the binary the plugin manifest pins. Nothing else ties
//! the two, so a tool renamed or dropped leaves the plugin steering the model to a call that does
//! not exist. Two binaries are checked:
//!
//! - the tree build, started through the launcher exactly as a host starts it (with downloads made
//!   impossible, so the launcher can run nothing but the build under test). Every name must be
//!   served and every tool must answer a minimal call with the fixture's content;
//! - the release the plugin manifests pin (`BASEMIND_SKEW_PINNED_BIN`), which is what a user who
//!   installs the plugin from `main` actually runs. Every tool, mode and call argument must be
//!   served by it. Main may name a surface that release lacks only while
//!   [`UNRELEASED_SURFACE_MARKER`] declares it, with a date, and the publish workflow refuses to
//!   release while that marker exists.
//!
//! Known mismatches live in [`KNOWN_MISMATCHES`]; the test prints each one on every run.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Directories and files of the repository that ship plugin content to a host. Each must exist and
/// yield at least one readable file: a root that silently disappears would shrink the gate.
const CONTENT_ROOTS: &[&str] = &[
    "hooks",
    "commands",
    "skills",
    ".claude-plugin",
    ".codex-plugin",
    ".cursor-plugin",
    "opencode-plugin",
    "plugin",
    // Only the extension ships; the rest of .pi/ is local, untracked agent governance.
    ".pi/extensions",
    "gemini-extension.json",
    "kimi.plugin.json",
    "settings.json",
    ".mcp.json",
    "scripts/mcp-launch.sh",
    "scripts/codex-mcp-launch.mjs",
];

const TEXT_EXTENSIONS: &[&str] = &[
    "md", "mdc", "json", "js", "mjs", "cjs", "ts", "py", "toml", "yaml", "yml", "cmd", "sh", "",
];

/// The plugin manifest whose `version` the launcher reads first.
#[cfg(unix)]
const PINNING_MANIFEST: &str = "plugin/.claude-plugin/plugin.json";

/// Declares that `main` names a tool surface the pinned release cannot serve. See the module docs.
#[cfg(unix)]
const UNRELEASED_SURFACE_MARKER: &str = ".github/unreleased-surface.toml";

/// Names the plugin content references although the binary does not serve them, each with the
/// reason it is tolerated. An entry that stops being a mismatch fails the test, so the list cannot
/// outlive the skew it records.
const KNOWN_MISMATCHES: &[(&str, &str)] = &[];

/// Result fields and values the plugin prose names that the fixture's minimal calls cannot produce
/// (a second page, a cut body, a feature this build may lack), with where the binary defines them.
/// An entry no plugin file uses any more fails the test.
const RESULT_VOCABULARY: &[(&str, &str)] = &[
    ("next_cursor", "every paged list (src/mcp/cursor.rs)"),
    (
        "omitted_bytes",
        "code_read / code_outline cut bodies (src/mcp/types_code.rs)",
    ),
    (
        "byte_budget",
        "code_grep truncated_reason GrepTruncation::ByteBudget (src/engine/ops/code_grep.rs)",
    ),
    ("chunk_idx", "docs_search hits (src/mcp/types_documents.rs)"),
    ("lanes_ran", "code_search (src/mcp/types_code.rs)"),
];

/// Identifiers that appear on a line naming a tool but are not part of any tool's vocabulary: shell
/// variables, helper names and prose in the plugin's scripts. An entry no file uses any more fails
/// the test.
const NOT_TOOL_VOCABULARY: &[&str] = &[
    "bash_alt",
    "cache_root",
    "grep_args",
    "grep_call",
    "mime_type",
    "open_store",
    "parse_config",
    "process_file",
    "scan_cap",
];

/// Top-level CLI groups an older binary had; a reference to one is a CLI reference, not prose.
const RETIRED_CLI_GROUPS: &[&str] = &["graph", "web", "shell"];

/// Per-tool minimal arguments for one successful call against the fixture repo, and a string its
/// answer must contain. A tool the server advertises without an entry here fails the test: add the
/// call that proves it answers. `None` marks the tools whose answer on this fixture has not been
/// pinned down (they need the `full` model stack); only their `isError` is checked.
fn minimal_call(tool: &str) -> Option<(Value, Option<&'static str>)> {
    Some(match tool {
        "code_outline" => (json!({"path": "src/lib.rs"}), Some("alpha")),
        "code_symbols" => (json!({"name": "alpha"}), Some("src/lib.rs")),
        "code_read" => (json!({"path": "src/lib.rs", "symbol": "alpha"}), Some("x + 1")),
        "code_grep" => (json!({"pattern": "alpha"}), Some("src/lib.rs")),
        "code_search" => (json!({"query": "increment a number"}), None),
        "code_refs" => (json!({"name": "alpha"}), Some("\"line\":6")),
        "find_files" => (json!({"query": "lib"}), Some("src/lib.rs")),
        "docs_search" => (json!({"query": "widget"}), Some("README.md")),
        "git_log" => (json!({}), Some("init")),
        "git_blame" => (json!({"path": "src/lib.rs"}), Some("src/lib.rs")),
        "git_diff" => (json!({}), Some("is_clean")),
        "memory_save" => (json!({"key": "skew/probe", "value": "x"}), None),
        "memory_search" => (json!({"query": "probe"}), None),
        "index_status" => (json!({}), Some("file_count")),
        "index_rescan" => (json!({}), Some("scanned")),
        "agent_post" => (json!({"subject": "skew probe", "body": "the gate's call"}), None),
        "agent_inbox" => (json!({}), None),
        "agent_threads" => (json!({}), None),
        _ => return None,
    })
}

/// A tool whose body needs a Cargo feature this build may lack: the call is then expected to fail
/// with a message naming the feature, and the gap is printed rather than failed.
fn feature_for(tool: &str) -> Option<(&'static str, bool)> {
    match tool {
        "code_search" => Some(("code-search", cfg!(feature = "code-search"))),
        "docs_search" => Some(("documents", cfg!(feature = "documents"))),
        "memory_save" | "memory_search" => Some(("memory", cfg!(feature = "memory"))),
        _ => None,
    }
}

// --- extraction ---------------------------------------------------------------------------------

/// Where each referenced name appears: name -> files naming it.
type References = BTreeMap<String, BTreeSet<String>>;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), "node_modules" | ".git" | "__pycache__" | "target") {
            continue;
        }
        collect_files(&entry.path(), out);
    }
}

const TOOL: &str = r"(?:(?:code|git|docs|memory|index|find)_[a-z][a-z_]*|agent_(?:post|inbox|threads))";

/// The top-level keys of the JSON-ish object that opens at `text[open]` (a `{`), with nested objects,
/// arrays and strings skipped. Keys may be quoted, escaped-quoted or bare (`{ mode: "post" }`).
fn object_keys(text: &str, open: usize) -> Vec<String> {
    let bytes = text.as_bytes();
    let key = Regex::new(r#"^\\?["']?([A-Za-z_][A-Za-z0-9_]*)\\?["']?\s*:"#).unwrap();
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut in_string: Option<u8> = None;
    let mut expect_key = false;
    let mut i = open;
    // A brace that never closes is prose, not an object: stop rather than read the rest of the file.
    let end = bytes.len().min(open + 4096);
    while i < end {
        let b = bytes[i];
        if let Some(quote) = in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'{' | b'[' => {
                depth += 1;
                expect_key = depth == 1 && b == b'{';
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            b',' if depth == 1 => expect_key = true,
            b'"' | b'\'' if !(depth == 1 && expect_key) => in_string = Some(b),
            _ if depth == 1 && expect_key && !b.is_ascii_whitespace() => {
                let rest = &text[i..];
                if let Some(caps) = key.captures(rest) {
                    keys.push(caps[1].to_string());
                    i += caps[0].len();
                    expect_key = false;
                    continue;
                }
                expect_key = false;
            }
            _ => {}
        }
        i += 1;
    }
    keys
}

/// The names one file's text refers to:
///
/// - a tool: `mcp__<server>__<tool>`, the `TOOL_NAMES="…"` list a hook prints, a snake_case name
///   from the verb-tool families (`code_`, `git_`, `docs_`, `memory_`, `index_`, `find_`) or one of
///   the three agent tools (`agent_post`, `agent_inbox`, `agent_threads`);
/// - `agents`, the removed coordination tool, named as a call (`agents {`, `agents mode`,
///   `` `agents` tool``), so content still teaching it fails. A bare `` `agents` `` is left alone:
///   it also names the `AGENTS.md` rules target;
/// - `<tool>.<argument>`: a top-level key of an argument object given to a tool, as a JSON example
///   (`code_grep {"pattern":"x"}`), a call (`code_grep({ pattern: "x" })`), or an MCP request
///   (`{"name": "code_grep", "arguments": {...}}`);
/// - `word:<identifier>`: a snake_case identifier on a line that names a tool. Prose describes
///   arguments and result fields this way (`scoped by subject, path_glob and members`); such a word
///   must be part of the served vocabulary: a tool, mode, argument or result field.
fn names_in(text: &str) -> BTreeSet<String> {
    let namespaced = Regex::new(r"mcp__(?:plugin_basemind_basemind|basemind)__([a-z][a-z0-9_]*)").unwrap();
    let list = Regex::new(r#"TOOL_NAMES="([^"]*)""#).unwrap();
    let family = Regex::new(&format!(r"\b({TOOL})\b")).unwrap();
    let agents = Regex::new(r"`agents` (?:tool|mode)|\bagents\s*\(?\s*\{|\bagents mode\b").unwrap();
    let call = Regex::new(&format!(r"\b({TOOL})\s*\(?\s*\{{")).unwrap();
    let request = Regex::new(&format!(
        r#"\\?"name\\?"\s*:\s*\\?"({TOOL})\\?"\s*,\s*\\?"arguments\\?"\s*:\s*\{{"#
    ))
    .unwrap();
    let line_tool = Regex::new(&format!(r"\b({TOOL})\b")).unwrap();
    let word = Regex::new(r"\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+)\b").unwrap();

    let mut names = BTreeSet::new();
    for caps in namespaced.captures_iter(text) {
        names.insert(caps[1].to_string());
    }
    for caps in list.captures_iter(text) {
        names.extend(caps[1].split_whitespace().map(str::to_string));
    }
    for caps in family.captures_iter(text) {
        names.insert(caps[1].to_string());
    }
    if agents.is_match(text) {
        names.insert("agents".to_string());
    }
    for caps in call.captures_iter(text).chain(request.captures_iter(text)) {
        let whole = caps.get(0).unwrap();
        for arg in object_keys(text, whole.end() - 1) {
            names.insert(format!("{}.{arg}", &caps[1]));
        }
    }
    for line in text.lines().filter(|line| line_tool.is_match(line)) {
        for caps in word.captures_iter(line) {
            if !family.is_match(&caps[1]) {
                names.insert(format!("word:{}", &caps[1]));
            }
        }
    }
    names
}

/// `basemind <group> [<sub>]` references in code: inline code and fenced blocks of Markdown, and
/// every line of the other files. A first word that is neither a command `top_level` lists nor a
/// retired group is prose ("basemind first"), not a command.
fn cli_refs_in(text: &str, markdown: bool, top_level: &BTreeSet<String>) -> BTreeSet<(String, Option<String>)> {
    let cli = Regex::new(r"\bbasemind(?:\.exe)?\s+([a-z][a-z-]*)(?:\s+([a-z][a-z_-]*))?").unwrap();
    let inline = Regex::new(r"`([^`\n]+)`").unwrap();
    let mut refs = BTreeSet::new();
    let mut fenced = false;
    for line in text.lines() {
        // A batch-file remark is prose even in a script.
        if line.trim_start().starts_with("REM ") {
            continue;
        }
        if markdown && line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let segments: Vec<&str> = if !markdown || fenced {
            vec![line]
        } else {
            inline.captures_iter(line).map(|c| c.get(1).unwrap().as_str()).collect()
        };
        for segment in segments {
            for caps in cli.captures_iter(segment) {
                let group = caps[1].to_string();
                if top_level.contains(&group) || RETIRED_CLI_GROUPS.contains(&group.as_str()) {
                    refs.insert((group, caps.get(2).map(|m| m.as_str().to_string())));
                }
            }
        }
    }
    refs
}

struct Content {
    refs: References,
    /// `(group, sub)` CLI references -> files naming them.
    cli: BTreeMap<(String, Option<String>), BTreeSet<String>>,
    files_read: usize,
}

/// Every name the plugin content under `root` references. Fails when a content root is missing or
/// yields no readable file, or when nothing was found: a gate that examined nothing proves nothing.
fn extract_references(root: &Path, content_roots: &[&str], top_level: &BTreeSet<String>) -> Result<Content, String> {
    let mut content = Content {
        refs: References::new(),
        cli: BTreeMap::new(),
        files_read: 0,
    };
    for rel in content_roots {
        let path = root.join(rel);
        if !path.exists() {
            return Err(format!(
                "content root {rel} is missing under {}: remove it from CONTENT_ROOTS only if the plugin no longer ships it",
                root.display()
            ));
        }
        let mut files = Vec::new();
        collect_files(&path, &mut files);
        let mut read_here = 0;
        for file in &files {
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !TEXT_EXTENSIONS.contains(&ext) {
                continue;
            }
            let Ok(text) = fs::read_to_string(file) else { continue };
            read_here += 1;
            let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
            for name in names_in(&text) {
                content.refs.entry(name).or_default().insert(shown.clone());
            }
            for cli in cli_refs_in(&text, matches!(ext, "md" | "mdc"), top_level) {
                content.cli.entry(cli).or_default().insert(shown.clone());
            }
        }
        if read_here == 0 {
            return Err(format!("content root {rel} holds no readable text file"));
        }
        content.files_read += read_here;
    }
    if content.refs.keys().all(|name| name.starts_with("word:")) {
        return Err(format!(
            "read {} plugin files and found no tool name in them",
            content.files_read
        ));
    }
    Ok(content)
}

/// What a binary serves: names (tools and `<tool>.<argument>`) and the vocabulary a
/// `word:` reference may use (those plus every mode, schema property and result field).
#[derive(Default)]
struct Served {
    names: BTreeSet<String>,
    words: BTreeSet<String>,
}

impl Served {
    fn from_tools(tools: &[Value]) -> Self {
        let mut served = Self::default();
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or_default().to_string();
            served.words.insert(name.clone());
            if let Some(props) = tool["inputSchema"]["properties"].as_object() {
                served.names.extend(props.keys().map(|arg| format!("{name}.{arg}")));
            }
            collect_keys(&tool["inputSchema"], &mut served.words);
            if let Some(modes) = tool["inputSchema"]["properties"]["mode"]["enum"].as_array() {
                served
                    .words
                    .extend(modes.iter().filter_map(Value::as_str).map(str::to_string));
            }
            served.names.insert(name);
        }
        served
    }

    fn serves(&self, name: &str) -> bool {
        match name.strip_prefix("word:") {
            Some(word) => {
                self.words.contains(word)
                    || NOT_TOOL_VOCABULARY.contains(&word)
                    || RESULT_VOCABULARY.iter().any(|(known, _)| *known == word)
            }
            None => self.names.contains(name),
        }
    }
}

/// Every object key anywhere in `value`.
fn collect_keys(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                out.insert(key.clone());
                collect_keys(inner, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_keys(item, out)),
        _ => {}
    }
}

/// Referenced names `served` lacks.
fn missing_from(refs: &References, served: &Served) -> Vec<String> {
    refs.keys().filter(|name| !served.serves(name)).cloned().collect()
}

fn describe(refs: &References, names: &[&String]) -> String {
    names
        .iter()
        .map(|n| {
            format!(
                "{n} (in {})",
                refs[n.as_str()].iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

// --- CLI ----------------------------------------------------------------------------------------

/// Subcommand names in a clap `--help` text, or empty when it lists none.
fn subcommands(help: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands {
            if !line.starts_with("  ") {
                break;
            }
            if let Some(name) = line.split_whitespace().next() {
                names.insert(name.to_string());
            }
        }
    }
    names.remove("help");
    names
}

fn help_of(fx: &Fixture, bin: &Path, args: &[&str]) -> Option<String> {
    let out = fx.command(bin).args(args).arg("--help").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The CLI references `bin` cannot run, as `basemind <group> [<sub>] (in files)` lines.
fn cli_mismatches(fx: &Fixture, bin: &Path, content: &Content) -> Vec<String> {
    let top = subcommands(&help_of(fx, bin, &[]).expect("basemind --help"));
    let mut groups: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    let mut bad = Vec::new();
    for ((group, sub), files) in &content.cli {
        let children = groups.entry(group.clone()).or_insert_with(|| {
            top.contains(group)
                .then(|| subcommands(&help_of(fx, bin, &[group]).unwrap_or_default()))
        });
        let ok = match (children, sub) {
            (None, _) => false,
            // A hidden alias (`code references` for `code refs`) is not listed but runs: ask clap.
            (Some(children), Some(sub)) if !children.is_empty() => {
                children.contains(sub) || help_of(fx, bin, &[group, sub]).is_some()
            }
            _ => true,
        };
        if !ok {
            let shown = sub.as_ref().map_or(group.clone(), |s| format!("{group} {s}"));
            bad.push(format!(
                "basemind {shown} (in {})",
                files.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    bad
}

// --- MCP stdio client ---------------------------------------------------------------------------

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write to the server");
        self.stdin.flush().expect("flush to the server");
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(timeout)
                .map_err(|e| format!("no reply to {method} within {timeout:?}: {e}"))?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg["id"] == json!(id) && msg.get("method").is_none() {
                if let Some(err) = msg.get("error") {
                    return Err(format!("{method} failed: {err}"));
                }
                return Ok(msg["result"].clone());
            }
        }
    }

    fn handshake(&mut self) -> Value {
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "plugin-skew-gate", "version": "0"}
                }),
                Duration::from_secs(120),
            )
            .expect("initialize");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    fn list_tools(&mut self) -> Vec<Value> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c}));
            let page = self
                .request("tools/list", params, Duration::from_secs(60))
                .expect("tools/list");
            tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_string()),
                None => return tools,
            }
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- fixture and launch -------------------------------------------------------------------------

/// A throwaway repository, data home, comms dir and home directory. Dropping it stops every daemon
/// the given binaries may have started, whether or not the test got that far.
struct Fixture {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
    comms: PathBuf,
    daemon_bins: Vec<PathBuf>,
}

impl Fixture {
    /// Every child process gets a throwaway data home, comms dir, home and cache, so no run can
    /// reach a daemon, index or configuration that belongs to a real session.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let program = Path::new(program.as_ref());
        let home = self.tmp.path().join("home");
        let mut cmd = Command::new(program);
        cmd.env("BASEMIND_DATA_HOME", &self.data)
            .env("BASEMIND_COMMS_DIR", &self.comms)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", self.tmp.path().join("xdg-cache"))
            // ~keep Windows resolves the platform cache dir under %USERPROFILE%, the throwaway home,
            // ~keep where it does not exist, so the grammar pack would find no cache dir at all.
            .env(
                "TREE_SITTER_LANGUAGE_PACK_CACHE_DIR",
                self.tmp.path().join("xdg-cache").join("tree-sitter-language-pack"),
            )
            .env("NO_COLOR", "1")
            .env_remove("BASEMIND_NO_AUTOSPAWN");
        // ~keep A Linux release loads its bundled lib/ through RUNPATH, which LD_LIBRARY_PATH
        // ~keep outranks: under CI's /usr/local/lib libheif the loader mixes the two sets and the
        // ~keep release cannot start. The build under test, which may need the variable, keeps it.
        if program != Path::new(BIN) && self.daemon_bins.iter().any(|bin| bin == program) {
            cmd.env_remove("LD_LIBRARY_PATH");
        }
        cmd
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for bin in &self.daemon_bins {
            let _ = self
                .command(bin)
                .arg("--root")
                .arg(&self.repo)
                .args(["comms", "stop"])
                .output();
        }
    }
}

fn fixture(scan_with: &Path) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(tmp.path().join("home")).unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n\npub fn beta() -> u32 {\n    alpha(2)\n}\n",
    )
    .unwrap();
    fs::write(repo.join("README.md"), "# Widget\n\nA widget project.\n").unwrap();
    fs::write(
        repo.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[documents]\nembed = false\n",
    )
    .unwrap();
    let data = tmp.path().join("data");
    let comms = tmp.path().join("comms");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&comms).unwrap();
    let fx = Fixture {
        tmp,
        repo,
        data,
        comms,
        daemon_bins: vec![scan_with.to_path_buf()],
    };
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&fx.repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e.x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e.x")
            .status()
            .expect("git in PATH");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["add", "src/lib.rs", "README.md", "basemind.toml"]);
    git(&["commit", "-qm", "init"]);
    let scan = fx
        .command(scan_with)
        .arg("--root")
        .arg(&fx.repo)
        .arg("scan")
        .output()
        .expect("scan the fixture");
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );
    fx
}

/// A directory whose only entries are the few tools the launcher needs before it execs
/// `BASEMIND_BIN`. With it as `PATH` the launcher has no `curl`, `wget` or `basemind` to fall back
/// on, so whatever answers is the binary under test or nothing.
#[cfg(unix)]
fn launcher_only_path(fx: &Fixture) -> PathBuf {
    let dir = fx.tmp.path().join("launcher-path");
    fs::create_dir_all(&dir).unwrap();
    for tool in ["sed", "awk", "head", "uname", "dirname"] {
        let found = ["/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(tool))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{tool} not found in /usr/bin or /bin"));
        std::os::unix::fs::symlink(found, dir.join(tool)).unwrap();
    }
    dir
}

/// The server command, started the way the plugin starts it. On Unix that is the launcher script
/// with `BASEMIND_BIN` pointing at the built binary and no download tool on `PATH`; on Windows the
/// launcher is a bash script a plain runner may lack, so the binary runs directly. Returns the
/// command and which of the two it is.
fn server_command(fx: &Fixture) -> (Command, &'static str) {
    #[cfg(unix)]
    {
        let launcher = repo_root().join("scripts").join("mcp-launch.sh");
        let bash = [
            "/bin/bash",
            "/usr/bin/bash",
            "/usr/local/bin/bash",
            "/opt/homebrew/bin/bash",
        ]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .expect("bash");
        let mut cmd = fx.command(bash);
        cmd.arg(launcher)
            .args(["serve", "--root"])
            .arg(&fx.repo)
            .env("BASEMIND_BIN", BIN)
            .env("CLAUDE_PLUGIN_ROOT", repo_root())
            .env("PATH", launcher_only_path(fx));
        (
            cmd,
            "scripts/mcp-launch.sh serve (BASEMIND_BIN pinned, no download tool on PATH)",
        )
    }
    #[cfg(not(unix))]
    {
        let mut cmd = fx.command(BIN);
        cmd.args(["serve", "--root"]).arg(&fx.repo);
        (cmd, "basemind serve (no bash launcher on this platform)")
    }
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
        .unwrap_or_default()
        .to_string()
}

fn top_level_commands(fx: &Fixture, bin: &Path) -> BTreeSet<String> {
    subcommands(&help_of(fx, bin, &[]).expect("basemind --help"))
}

/// Partition `missing` into tolerated (listed in [`KNOWN_MISMATCHES`]) and unexpected names, and
/// fail on a listed entry that is no longer a mismatch.
fn tolerate_known<'a>(missing: &'a [String], out: &mut impl Write, refs: &References) -> Vec<&'a String> {
    let known: BTreeMap<&str, &str> = KNOWN_MISMATCHES.iter().copied().collect();
    let (tolerated, unexpected): (Vec<&String>, Vec<&String>) =
        missing.iter().partition(|name| known.contains_key(name.as_str()));
    for name in &tolerated {
        let _ = writeln!(
            out,
            "plugin-skew: KNOWN MISMATCH {name}: {} (named in {} files, e.g. {})",
            known[name.as_str()],
            refs[name.as_str()].len(),
            refs[name.as_str()].iter().next().unwrap()
        );
    }
    let stale: Vec<&&str> = known
        .keys()
        .filter(|name| !missing.iter().any(|m| m == *name))
        .collect();
    assert!(
        stale.is_empty(),
        "KNOWN_MISMATCHES entries that are no longer mismatches: {stale:?}"
    );
    unexpected
}

// --- the gate: the tree build -------------------------------------------------------------------

#[test]
fn plugin_content_names_only_tools_the_binary_serves_and_each_answers() {
    let fx = fixture(Path::new(BIN));
    let top = top_level_commands(&fx, Path::new(BIN));
    let content = extract_references(&repo_root(), CONTENT_ROOTS, &top).expect("plugin content extraction");
    let refs = &content.refs;
    let (cmd, how) = server_command(&fx);
    let mut server = Mcp::spawn(cmd);
    server.handshake();
    let tools = server.list_tools();
    let mut served = Served::from_tools(&tools);
    let tool_names: Vec<String> = tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();

    let out = &mut std::io::stderr().lock();

    // One minimal call per advertised tool, whose answer must carry the fixture's content. The
    // answers' field names join the vocabulary prose may use.
    let mut failures = Vec::new();
    let mut answered = 0;
    let mut content_checked = 0;
    for name in &tool_names {
        let Some((args, expect)) = minimal_call(name) else {
            failures.push(format!("{name}: advertised but the gate has no minimal call for it"));
            continue;
        };
        let reply = server.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            Duration::from_secs(120),
        );
        match reply {
            Err(e) => failures.push(format!("{name}: {e}")),
            Ok(result) if result["isError"] == json!(true) => {
                let text = result_text(&result);
                match feature_for(name) {
                    Some((feature, false)) if text.contains(&format!("`{feature}`")) => {
                        let _ = writeln!(
                            out,
                            "plugin-skew: {name} not exercised, built without feature `{feature}`: {text}"
                        );
                    }
                    Some((feature, false)) => failures.push(format!(
                        "{name}: built without `{feature}`, but the error does not name the feature: {text}"
                    )),
                    _ => failures.push(format!("{name}: isError result: {text}")),
                }
            }
            Ok(result) => {
                let text = result_text(&result);
                collect_keys(&result["structuredContent"], &mut served.words);
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    collect_keys(&parsed, &mut served.words);
                }
                match expect {
                    Some(marker) if !text.contains(marker) => failures.push(format!(
                        "{name}: answered without the fixture's `{marker}`: {}",
                        text.chars().take(400).collect::<String>()
                    )),
                    Some(_) => {
                        answered += 1;
                        content_checked += 1;
                    }
                    None => answered += 1,
                }
            }
        }
    }

    let _ = writeln!(
        out,
        "plugin-skew: launched via {how}; it advertises {} tools: {:?}",
        tool_names.len(),
        tool_names
    );
    let _ = writeln!(
        out,
        "plugin-skew: {} plugin files read; {} distinct tool, mode, argument and word references; {} CLI references",
        content.files_read,
        refs.len(),
        content.cli.len()
    );
    let _ = writeln!(
        out,
        "plugin-skew: {answered} of {} advertised tools answered a minimal call ({content_checked} with the fixture's content)",
        tool_names.len()
    );

    assert!(!tool_names.is_empty(), "the server advertised no tools");
    for tool in ["agent_post", "agent_inbox", "agent_threads"] {
        assert!(served.names.contains(tool), "a comms build advertises `{tool}`");
    }
    assert!(
        refs.keys().filter(|n| !n.starts_with("word:")).count() >= 10,
        "only {} tool names extracted: the extraction is broken",
        refs.len()
    );
    assert!(failures.is_empty(), "tool calls failed:\n{}", failures.join("\n"));
    assert!(
        content_checked >= 10,
        "only {content_checked} answers were checked for the fixture's content"
    );

    let missing = missing_from(refs, &served);
    let unexpected = tolerate_known(&missing, out, refs);
    assert!(
        unexpected.is_empty(),
        "the plugin content names tools, modes, arguments or fields the server does not serve: {}",
        describe(refs, &unexpected)
    );
    let unused: Vec<&&str> = NOT_TOOL_VOCABULARY
        .iter()
        .chain(RESULT_VOCABULARY.iter().map(|(word, _)| word))
        .filter(|w| !refs.contains_key(&format!("word:{w}")))
        .collect();
    assert!(
        unused.is_empty(),
        "NOT_TOOL_VOCABULARY / RESULT_VOCABULARY entries no plugin file uses any more: {unused:?}"
    );

    let cli_bad = cli_mismatches(&fx, Path::new(BIN), &content);
    assert!(
        cli_bad.is_empty(),
        "the plugin content names CLI commands this binary lacks:\n{}",
        cli_bad.join("\n")
    );
}

// --- the gate: the release the manifests pin ------------------------------------------------------

/// Today as `YYYY-MM-DD` (UTC), from the system clock.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's days-to-civil conversion.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(unix)]
struct UnreleasedSurface {
    pinned_version: String,
    since: String,
    review_by: String,
    reason: String,
}

#[cfg(unix)]
fn unreleased_surface(root: &Path) -> Option<UnreleasedSurface> {
    let text = fs::read_to_string(root.join(UNRELEASED_SURFACE_MARKER)).ok()?;
    let table: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{UNRELEASED_SURFACE_MARKER}: {e}"));
    let field = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{UNRELEASED_SURFACE_MARKER} needs a string `{key}`"))
            .to_string()
    };
    let date = Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
    let marker = UnreleasedSurface {
        pinned_version: field("pinned_version"),
        since: field("since"),
        review_by: field("review_by"),
        reason: field("reason"),
    };
    assert!(
        date.is_match(&marker.since) && date.is_match(&marker.review_by),
        "{UNRELEASED_SURFACE_MARKER}: `since` and `review_by` must be YYYY-MM-DD dates"
    );
    Some(marker)
}

#[cfg(unix)]
fn manifest_version(root: &Path) -> String {
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(root.join(PINNING_MANIFEST)).unwrap_or_else(|e| panic!("read {PINNING_MANIFEST}: {e}")),
    )
    .unwrap();
    manifest["version"]
        .as_str()
        .unwrap_or_else(|| panic!("{PINNING_MANIFEST} has no version"))
        .to_string()
}

/// The binary of the release the plugin manifests pin. `BASEMIND_SKEW_PINNED_BIN` names it
/// (`scripts/fetch-release-binaries.sh` downloads it), or is `unreleased` when the pinned version
/// is the one this tree is about to release, whose binary is the tree build itself.
#[cfg(unix)]
fn pinned_binary(pinned: &str) -> Option<PathBuf> {
    match std::env::var("BASEMIND_SKEW_PINNED_BIN") {
        Ok(value) if value == "unreleased" => {
            assert_eq!(
                pinned,
                env!("CARGO_PKG_VERSION"),
                "BASEMIND_SKEW_PINNED_BIN=unreleased, but the manifests pin {pinned}, not this tree's version"
            );
            Some(PathBuf::from(BIN))
        }
        Ok(value) => {
            let path = PathBuf::from(value);
            assert!(
                path.is_file(),
                "BASEMIND_SKEW_PINNED_BIN={} is not a file",
                path.display()
            );
            Some(path)
        }
        Err(_) => {
            assert!(
                std::env::var_os("BASEMIND_TEST_ALLOW_NO_PINNED_RELEASE").is_some(),
                "BASEMIND_SKEW_PINNED_BIN is unset, so the release the plugin manifests pin ({pinned}) was not \
                 checked. Run `scripts/fetch-release-binaries.sh <dir> <triple> {pinned}` and point the variable \
                 at the binary, or set BASEMIND_TEST_ALLOW_NO_PINNED_RELEASE=1 to skip this check here."
            );
            let _ = writeln!(
                std::io::stderr().lock(),
                "SKIP (BASEMIND_TEST_ALLOW_NO_PINNED_RELEASE): the pinned release {pinned} was not checked"
            );
            None
        }
    }
}

#[cfg(unix)]
#[test]
fn the_release_the_manifests_pin_serves_what_the_plugin_names() {
    let root = repo_root();
    let pinned = manifest_version(&root);
    let Some(bin) = pinned_binary(&pinned) else { return };
    let fx = fixture(&bin);
    let reported = fx
        .command(&bin)
        .arg("--version")
        .output()
        .expect("run the pinned binary");
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    assert_eq!(
        reported,
        format!("basemind {pinned}"),
        "BASEMIND_SKEW_PINNED_BIN is not the release the manifests pin"
    );

    let top = top_level_commands(&fx, &bin);
    let content = extract_references(&root, CONTENT_ROOTS, &top).expect("plugin content extraction");
    let mut cmd = fx.command(&bin);
    cmd.args(["serve", "--root"]).arg(&fx.repo);
    let mut server = Mcp::spawn(cmd);
    server.handshake();
    let served = Served::from_tools(&server.list_tools());
    drop(server);

    // Tools and call arguments. Prose words are the tree test's job, since a release
    // cannot be asked for result fields it may name differently; so are the KNOWN_MISMATCHES.
    let mut missing: Vec<String> = missing_from(&content.refs, &served)
        .into_iter()
        .filter(|n| !n.starts_with("word:") && !KNOWN_MISMATCHES.iter().any(|(known, _)| known == n))
        .collect();
    missing.extend(cli_mismatches(&fx, &bin, &content));

    let out = &mut std::io::stderr().lock();
    let marker = unreleased_surface(&root);
    let _ = writeln!(
        out,
        "plugin-skew (pinned {pinned}): {} of {} referenced names and CLI commands are not served by {}",
        missing.len(),
        content.refs.keys().filter(|n| !n.starts_with("word:")).count() + content.cli.len(),
        bin.display()
    );
    match marker {
        None => assert!(
            missing.is_empty(),
            "the plugin manifests pin {pinned}, which does not serve what the plugin content names; a user who \
             installs the plugin from main gets calls that fail. Unserved: {}",
            missing.join("; ")
        ),
        Some(marker) => {
            assert_eq!(
                marker.pinned_version, pinned,
                "{UNRELEASED_SURFACE_MARKER} was written for {}, but the manifests now pin {pinned}: remove or renew it",
                marker.pinned_version
            );
            assert!(
                !missing.is_empty(),
                "{UNRELEASED_SURFACE_MARKER} declares an unreleased surface, but {pinned} serves everything the plugin \
                 names: remove the marker"
            );
            let today = today();
            assert!(
                today <= marker.review_by,
                "{UNRELEASED_SURFACE_MARKER} (since {}) expired on {}: release, or renew it with a new date. Unserved by \
                 {pinned}: {}",
                marker.since,
                marker.review_by,
                missing.join("; ")
            );
            let _ = writeln!(
                out,
                "plugin-skew (pinned {pinned}): UNRELEASED SURFACE since {} until {} ({}). Unserved: {}",
                marker.since,
                marker.review_by,
                marker.reason,
                missing.join("; ")
            );
        }
    }
}

// --- the extraction itself ----------------------------------------------------------------------

fn no_top_level() -> BTreeSet<String> {
    ["code", "git", "status"].iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_referenced_name_the_server_lacks_is_reported() {
    let mut refs = References::new();
    refs.entry("code_teleport".to_string())
        .or_default()
        .insert("injected/by-the-test.md".to_string());
    refs.entry("word:path_glob".to_string())
        .or_default()
        .insert("injected/by-the-test.md".to_string());
    let served = Served::from_tools(&[json!({"name": "code_outline", "inputSchema": {"properties": {"path": {}}}})]);
    let missing = missing_from(&refs, &served);
    assert!(missing.contains(&"code_teleport".to_string()), "{missing:?}");
    assert!(missing.contains(&"word:path_glob".to_string()), "{missing:?}");
}

fn content_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for root in CONTENT_ROOTS {
        let path = dir.path().join(root);
        if Path::new(root).extension().is_some() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "{}\n").unwrap();
        } else {
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("README.md"), "prose\n").unwrap();
        }
    }
    for (rel, text) in files {
        fs::write(dir.path().join(rel), text).unwrap();
    }
    dir
}

#[test]
fn extracting_no_names_is_an_error_not_a_pass() {
    let empty = tempfile::tempdir().unwrap();
    let err = extract_references(empty.path(), CONTENT_ROOTS, &no_top_level())
        .err()
        .unwrap();
    assert!(err.contains("content root hooks is missing"), "{err}");
    let prose = content_tree(&[("skills/a.md", "no tools here\n")]);
    let err = extract_references(prose.path(), CONTENT_ROOTS, &no_top_level())
        .err()
        .unwrap();
    assert!(err.contains("found no tool name"), "{err}");
}

#[test]
fn a_missing_or_empty_content_root_is_an_error() {
    let tree = content_tree(&[("skills/a.md", "call code_outline\n")]);
    fs::remove_dir_all(tree.path().join(".pi/extensions")).unwrap();
    let err = extract_references(tree.path(), CONTENT_ROOTS, &no_top_level())
        .err()
        .unwrap();
    assert!(err.contains("content root .pi/extensions is missing"), "{err}");
    fs::create_dir_all(tree.path().join(".pi/extensions")).unwrap();
    let err = extract_references(tree.path(), CONTENT_ROOTS, &no_top_level())
        .err()
        .unwrap();
    assert!(
        err.contains("content root .pi/extensions holds no readable text file"),
        "{err}"
    );
}

#[test]
fn every_spelling_of_a_tool_name_is_recognised() {
    let text = "call mcp__plugin_basemind_basemind__code_outline; TOOL_NAMES=\"find_files agents zzz_odd\"; \
                use git_log {} and `agents` mode `post`; mcp__basemind__docs_search; git_log {\"rev\":\"x\"} \
                code_grep {\"pattern\":\"a\",\"regex\":true} agent_post {\"subject\":\"s\",\"body\":\"b\"} \
                read with agent_inbox; mcp__plugin_basemind_basemind__agent_threads; comms.agent_id";
    let names = names_in(text);
    for want in [
        "code_outline",
        "find_files",
        "agents",
        "zzz_odd",
        "git_log",
        "docs_search",
        "git_log.rev",
        "code_grep.regex",
        "agent_post",
        "agent_post.subject",
        "agent_inbox",
        "agent_threads",
    ] {
        assert!(names.contains(want), "{want} missing from {names:?}");
    }
    assert!(!names.contains("agent_id"), "a config key is not a tool: {names:?}");
}

#[test]
fn arguments_are_read_from_every_call_form() {
    let text = r#"
        code_grep({ pattern: "x", filter: { language: "rust" } })
        await client.callTool({"name": "code_read", "arguments": {"path": "a", "range": {"start": 1}}})
        agent_post({ subject: "s", body: "b", paths: ["src/**"] })
        code_symbols {\"name\":\"alpha\"}
    "#;
    let names = names_in(text);
    for want in [
        "code_grep.pattern",
        "code_grep.filter",
        "code_read.path",
        "code_read.range",
        "agent_post.subject",
        "agent_post.body",
        "agent_post.paths",
        "code_symbols.name",
    ] {
        assert!(names.contains(want), "{want} missing from {names:?}");
    }
    for nested in ["code_grep.language", "code_read.start"] {
        assert!(
            !names.contains(nested),
            "{nested}: a nested key is not an argument: {names:?}"
        );
    }
}

#[test]
fn prose_words_next_to_a_tool_are_vocabulary_references() {
    let names =
        names_in("Use agent_threads to list threads scoped by subject, path_glob and members.\nno tool: other_word\n");
    assert!(names.contains("word:path_glob"), "{names:?}");
    assert!(
        !names.contains("word:other_word"),
        "a line without a tool name is not read: {names:?}"
    );
}

#[test]
fn cli_references_come_from_code_not_prose() {
    let top: BTreeSet<String> = ["code", "git", "status"].iter().map(|s| s.to_string()).collect();
    assert!(cli_refs_in("REM wrapper for basemind code scripts\n", false, &top).is_empty());
    let md = "Run `basemind code outline src/a.rs` or `basemind graph map`.\nbasemind first, grep second.\n\
              ```sh\nbasemind git log\n```\n";
    let refs = cli_refs_in(md, true, &top);
    for want in [("code", Some("outline")), ("graph", Some("map")), ("git", Some("log"))] {
        assert!(
            refs.contains(&(want.0.to_string(), want.1.map(String::from))),
            "{want:?} missing from {refs:?}"
        );
    }
    assert!(
        !refs.iter().any(|(g, _)| g == "first"),
        "prose is not a command: {refs:?}"
    );
}

#[test]
fn clap_help_lists_subcommands() {
    let help = "Usage: basemind code <COMMAND>\n\nCommands:\n  outline  Show\n  read     Read\n  help     Print\n\nOptions:\n  -h\n";
    let names = subcommands(help);
    assert_eq!(names, ["outline", "read"].iter().map(|s| s.to_string()).collect());
}

#[test]
fn today_is_an_iso_date() {
    let today = today();
    assert!(
        Regex::new(r"^20\d{2}-\d{2}-\d{2}$").unwrap().is_match(&today),
        "{today}"
    );
}
