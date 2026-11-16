//! Plugin skew gate: every MCP tool the shipped plugin content names must be a tool the binary
//! serves, and every tool the binary serves must answer one minimal call.
//!
//! The plugin's hooks, skills, commands and rules tell the model which tools to call; the plugin
//! launcher (`scripts/mcp-launch.sh`) starts a version-pinned binary. Nothing else ties the two, so
//! a tool renamed or dropped in the binary leaves the plugin steering the model to a call that does
//! not exist. This test starts the binary the way the launcher does (`BASEMIND_BIN` pins the
//! launcher to the built binary, so nothing is downloaded), lists its tools and compares.
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

/// Directories and files of the repository that ship plugin content to a host.
const CONTENT_ROOTS: &[&str] = &[
    "hooks",
    "commands",
    "skills",
    ".claude-plugin",
    ".codex-plugin",
    ".cursor-plugin",
    "opencode-plugin",
    "plugin",
    "gemini-extension.json",
    "kimi.plugin.json",
    "settings.json",
];

const TEXT_EXTENSIONS: &[&str] = &["md", "json", "js", "mjs", "py", "toml", "yaml", "yml", "cmd", "sh", ""];

/// Tool names the plugin content references although the binary does not advertise them, each with
/// the reason it is tolerated. An entry that stops being a mismatch fails the test, so the list
/// cannot outlive the skew it records.
const KNOWN_MISMATCHES: &[(&str, &str)] = &[];

/// Per-tool minimal arguments for one successful call against the fixture repo. A tool the server
/// advertises without an entry here fails the test: add the call that proves it answers.
fn minimal_call(tool: &str) -> Option<Value> {
    Some(match tool {
        "code_outline" => json!({"path": "src/lib.rs"}),
        "code_symbols" => json!({"name": "alpha"}),
        "code_read" => json!({"path": "src/lib.rs", "symbol": "alpha"}),
        "code_grep" => json!({"pattern": "alpha"}),
        "code_search" => json!({"query": "increment a number"}),
        "code_refs" => json!({"name": "alpha"}),
        "find_files" => json!({"query": "lib"}),
        "docs_search" => json!({"query": "widget"}),
        "git_log" => json!({}),
        "git_blame" => json!({"path": "src/lib.rs"}),
        "git_diff" => json!({}),
        "memory_save" => json!({"key": "skew/probe", "value": "x"}),
        "memory_search" => json!({"query": "probe"}),
        "index_status" => json!({}),
        "index_rescan" => json!({}),
        "agent_post" => json!({"subject": "skew probe", "body": "the gate's call"}),
        "agent_inbox" => json!({}),
        "agent_threads" => json!({}),
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

/// Where each referenced tool name appears: name -> files naming it.
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

/// The tool names one file's text refers to. Four spellings are recognised:
///
/// - `mcp__<server>__<tool>`, the host-namespaced form;
/// - the `TOOL_NAMES="…"` list a hook prints to the model;
/// - a snake_case name from the verb-tool families (`code_`, `git_`, `docs_`, `memory_`, `index_`,
///   `find_`), the shape every verb tool has, and the three agent tools (`agent_post`,
///   `agent_inbox`, `agent_threads`);
/// - `agents`, the removed coordination tool, named as a call (`agents {`, `agents mode`,
///   `` `agents` tool``), so content still teaching it fails. A bare `` `agents` `` is left alone:
///   it also names the `AGENTS.md` rules target.
///
/// An argument in a JSON call example (`code_grep {"pattern":"x","regex":true}`) is recorded as
/// `<tool>.<argument>`, because the plugin can be wrong about an argument as well as a tool.
fn names_in(text: &str) -> BTreeSet<String> {
    let namespaced = Regex::new(r"mcp__(?:plugin_basemind_basemind|basemind)__([a-z][a-z0-9_]*)").unwrap();
    let list = Regex::new(r#"TOOL_NAMES="([^"]*)""#).unwrap();
    let family =
        Regex::new(r"\b((?:code|git|docs|memory|index|find)_[a-z][a-z_]*|agent_(?:post|inbox|threads))\b").unwrap();
    let agents = Regex::new(r"`agents` (?:tool|mode)|\bagents\s*\{|\bagents mode\b").unwrap();
    let call =
        Regex::new(r"\b((?:code|git|docs|memory|index|find)_[a-z][a-z_]*|agent_(?:post|inbox|threads))\s*\{([^{}]*)\}")
            .unwrap();
    let key = Regex::new(r#"\\?"([a-z_]+)\\?"\s*:"#).unwrap();

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
    for caps in call.captures_iter(text) {
        for arg in key.captures_iter(&caps[2]) {
            names.insert(format!("{}.{}", &caps[1], &arg[1]));
        }
    }
    names
}

/// Every tool name the plugin content under `root` references, plus how many files were read.
/// Fails when nothing was read or nothing was found: a gate that examined nothing proves nothing.
fn extract_references(root: &Path, content_roots: &[&str]) -> Result<(References, usize), String> {
    let mut files = Vec::new();
    for rel in content_roots {
        collect_files(&root.join(rel), &mut files);
    }
    let mut refs = References::new();
    let mut read = 0;
    for file in &files {
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !TEXT_EXTENSIONS.contains(&ext) {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else { continue };
        read += 1;
        let rel = file.strip_prefix(root).unwrap_or(file).display().to_string();
        for name in names_in(&text) {
            refs.entry(name).or_default().insert(rel.clone());
        }
    }
    if read == 0 {
        return Err(format!("read no plugin content files under {}", root.display()));
    }
    if refs.is_empty() {
        return Err(format!("read {read} plugin files and found no tool name in them"));
    }
    Ok((refs, read))
}

/// Referenced names absent from `advertised`.
fn missing_from(refs: &References, advertised: &BTreeSet<String>) -> Vec<String> {
    refs.keys()
        .filter(|name| !advertised.contains(*name))
        .cloned()
        .collect()
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
            .stderr(Stdio::null())
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

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
    comms: PathBuf,
}

impl Fixture {
    /// Every child process gets a throwaway data home and comms dir, so no run can reach a daemon
    /// or index that belongs to a real session.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("BASEMIND_DATA_HOME", &self.data)
            .env("BASEMIND_COMMS_DIR", &self.comms)
            .env("XDG_CACHE_HOME", self._tmp.path().join("xdg-cache"))
            .env("NO_COLOR", "1")
            .env_remove("BASEMIND_NO_AUTOSPAWN");
        cmd
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
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
        _tmp: tmp,
        repo,
        data,
        comms,
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
        .command(BIN)
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

/// The server command, started the way the plugin starts it. On Unix that is the launcher script
/// with `BASEMIND_BIN` pointing at the built binary (the launcher honours that pin before any
/// download); on Windows the launcher is a bash script a plain runner may lack, so the binary runs
/// directly. Returns the command and which of the two it is.
fn server_command(fx: &Fixture) -> (Command, &'static str) {
    if cfg!(unix) {
        let launcher = repo_root().join("scripts").join("mcp-launch.sh");
        let mut cmd = fx.command("bash");
        cmd.arg(launcher)
            .args(["serve", "--root"])
            .arg(&fx.repo)
            .env("BASEMIND_BIN", BIN)
            .env("CLAUDE_PLUGIN_ROOT", repo_root());
        (cmd, "scripts/mcp-launch.sh serve (BASEMIND_BIN pinned)")
    } else {
        let mut cmd = fx.command(BIN);
        cmd.args(["serve", "--root"]).arg(&fx.repo);
        (cmd, "basemind serve (no bash launcher on this platform)")
    }
}

fn stop_daemons(fx: &Fixture) {
    let _ = fx
        .command(BIN)
        .arg("--root")
        .arg(&fx.repo)
        .args(["comms", "stop"])
        .output();
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
        .unwrap_or_default()
        .to_string()
}

// --- the gate -----------------------------------------------------------------------------------

#[test]
fn plugin_content_names_only_tools_the_binary_serves_and_each_answers() {
    let (refs, files_read) = extract_references(&repo_root(), CONTENT_ROOTS).expect("plugin content extraction");
    let fx = fixture();
    let (cmd, how) = server_command(&fx);
    let mut server = Mcp::spawn(cmd);
    let init = server.handshake();
    let tools = server.list_tools();
    let mut advertised: BTreeSet<String> = tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    let tool_count = advertised.len();
    for tool in &tools {
        let name = tool["name"].as_str().unwrap_or_default();
        if let Some(props) = tool["inputSchema"]["properties"].as_object() {
            advertised.extend(props.keys().map(|arg| format!("{name}.{arg}")));
        }
    }

    // The reference line CI logs show; `--nocapture` is not needed because it goes to the stream.
    let out = &mut std::io::stderr().lock();
    let _ = writeln!(
        out,
        "plugin-skew: launched via {how}; server {} advertises {tool_count} tools (plus their argument names): {:?}",
        init["serverInfo"]["version"],
        advertised.iter().filter(|n| !n.contains('.')).collect::<Vec<_>>()
    );
    let _ = writeln!(
        out,
        "plugin-skew: {files_read} plugin files read, {} distinct tool and call-argument names referenced: {:?}",
        refs.len(),
        refs.keys().collect::<Vec<_>>()
    );

    assert!(tool_count > 0, "the server advertised no tools");
    if cfg!(feature = "comms") {
        for tool in ["agent_post", "agent_inbox", "agent_threads"] {
            assert!(advertised.contains(tool), "a comms build advertises `{tool}`");
        }
    }
    assert!(
        refs.len() >= 10,
        "only {} tool names extracted: the extraction is broken",
        refs.len()
    );

    let known: BTreeMap<&str, &str> = KNOWN_MISMATCHES.iter().copied().collect();
    let missing = missing_from(&refs, &advertised);
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
    assert!(
        unexpected.is_empty(),
        "the plugin content names tools the server does not advertise: {}",
        unexpected
            .iter()
            .map(|n| format!(
                "{n} (in {})",
                refs[n.as_str()].iter().cloned().collect::<Vec<_>>().join(", ")
            ))
            .collect::<Vec<_>>()
            .join("; ")
    );

    // One minimal successful call per advertised tool.
    let mut failures = Vec::new();
    let mut answered = 0;
    for name in advertised.iter().filter(|n| !n.contains([':', '.'])) {
        let Some(args) = minimal_call(name) else {
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
                    Some((feature, false)) => {
                        let _ = writeln!(
                            out,
                            "plugin-skew: {name} not exercised, built without feature `{feature}`: {text}"
                        );
                    }
                    _ => failures.push(format!("{name}: isError result: {text}")),
                }
            }
            Ok(_) => answered += 1,
        }
    }
    let _ = writeln!(
        out,
        "plugin-skew: {answered} of {} advertised tools answered a minimal call",
        tool_count
    );
    stop_daemons(&fx);
    assert!(failures.is_empty(), "tool calls failed:\n{}", failures.join("\n"));
    assert!(answered > 0, "no tool answered a call");
}

#[test]
fn a_referenced_name_the_server_lacks_is_reported() {
    let (mut refs, _) = extract_references(&repo_root(), CONTENT_ROOTS).expect("plugin content extraction");
    refs.entry("code_teleport".to_string())
        .or_default()
        .insert("injected/by-the-test.md".to_string());
    let advertised: BTreeSet<String> = ["code_outline".to_string()].into();
    let missing = missing_from(&refs, &advertised);
    assert!(missing.contains(&"code_teleport".to_string()), "{missing:?}");
}

#[test]
fn extracting_no_names_is_an_error_not_a_pass() {
    let empty = tempfile::tempdir().unwrap();
    assert!(extract_references(empty.path(), CONTENT_ROOTS).is_err());
    let prose = tempfile::tempdir().unwrap();
    fs::create_dir_all(prose.path().join("skills")).unwrap();
    fs::write(prose.path().join("skills/a.md"), "no tools here\n").unwrap();
    assert!(extract_references(prose.path(), CONTENT_ROOTS).is_err());
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
