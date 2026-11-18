//! The user's own MCP servers, which Codex and OpenCode start from their user-level configuration
//! beside the injected one. Both harnesses merge a launch's configuration into the user's, so
//! nothing a launch passes removes a server: each has to be switched off by name. The names are
//! read at launch, from the harness itself (Codex) or from the configuration files (OpenCode).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(not(unix))]
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::error::LaunchError;
use crate::session::CommandPlan;

/// Codex features that load MCP servers the user's `mcp_servers` table does not list: plugins
/// (whose servers `codex mcp list` leaves out once they are off) and the ChatGPT apps
/// connectors. Codex 0.169 takes them as `-c features.<name>=false`.
pub(crate) const CODEX_FEATURES_OFF: [&str; 4] = ["-c", "features.plugins=false", "-c", "features.apps=false"];

/// How long `codex mcp list` may take before the launch is refused.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the listing's output may stay open once its process group is gone.
const READ_GRACE: Duration = Duration::from_secs(2);
/// The most output `codex mcp list` or an OpenCode configuration file may have.
const MAX_BYTES: u64 = 1024 * 1024;
/// The command OpenCode is given for each server it switches off. OpenCode 2.3.2 dropped a
/// server entry that names `enabled` alone, so the entry is a whole one; it is never run.
const NEVER_RUN: &str = "/usr/bin/true";
/// The files OpenCode 2.3.2 reads from a configuration directory and from each ancestor of its
/// working directory (its `Sr`, checked in the 2.3.2 bundle; it reads no `config.json`).
const OPENCODE_FILES: [&str; 2] = ["opencode.json", "opencode.jsonc"];
/// The subdirectories of a configuration directory OpenCode 2.3.2 reads agent Markdown files
/// from, and whether it descends into their subdirectories (`{agent,agents}/**/*.md`,
/// `{mode,modes}/*.md`).
const AGENT_DIRS: [(&str, bool); 4] = [("agent", true), ("agents", true), ("mode", false), ("modes", false)];
/// How deep below an agent directory the Markdown files are looked for.
const MAX_AGENT_DEPTH: usize = 16;
/// How many entries the agent directories of one configuration directory may hold.
const MAX_AGENT_FILES: usize = 4096;

fn refused(harness: &'static str, reason: impl Into<String>) -> LaunchError {
    LaunchError::UserMcpServers {
        harness,
        reason: reason.into(),
    }
}

/// The user's Codex MCP servers, as `<codex> -c features.plugins=false -c features.apps=false mcp
/// list --json` reports them in the plan's environment and working directory: the merged
/// configuration Codex itself would start them from, managed configuration included.
///
/// The listing runs with the session's environment, launch token included, outside the session's
/// process group and keeper: it is the same executable the session then runs with the same
/// environment, so it is trusted as the harness is. It gets a session and process group of its
/// own, killed whole once it exits or times out: an npm-installed `codex` is a Node shim whose
/// native child holds the output pipe, which killing the shim alone left open.
pub(crate) fn codex_servers(plan: &CommandPlan) -> Result<Vec<String>, LaunchError> {
    codex_servers_within(plan, LIST_TIMEOUT)
}

/// [`codex_servers`] with the listing given `timeout`.
pub(crate) fn codex_servers_within(plan: &CommandPlan, timeout: Duration) -> Result<Vec<String>, LaunchError> {
    let mut command = Command::new(&plan.program);
    command
        .args(CODEX_FEATURES_OFF)
        .args(["mcp", "list", "--json"])
        .env_clear()
        .envs(&plan.env)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::process::isolate(&mut command);
    let mut child = command.spawn().map_err(|source| LaunchError::Spawn {
        executable: plan.program.clone(),
        source,
    })?;
    let stdout = child.stdout.take();
    let (sender, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            // Past the cap the rest is not read; the parse then fails.
            let _ = stdout.take(MAX_BYTES + 1).read_to_end(&mut out);
        }
        let _ = sender.send(out);
    });
    let exited = wait_for_exit(&mut child, timeout);
    // What is left of the listing's group goes before the leader is reaped, while the group's
    // id is still ours.
    #[cfg(unix)]
    if exited.is_some() {
        crate::process::kill_group(child.id());
    }
    let _ = child.kill();
    let status = child.wait().ok();
    // A process that left the group may still hold the pipe; the reader is then left behind.
    let out = output.recv_timeout(READ_GRACE).ok();
    match (exited, status, out) {
        (Some(false), ..) => Err(refused("codex", "`codex mcp list` did not finish in time")),
        (Some(true), Some(status), Some(out)) if status.success() => parse_codex_list(&out),
        (Some(true), Some(status), None) if status.success() => {
            Err(refused("codex", "`codex mcp list` left its output open"))
        }
        _ => Err(refused("codex", "`codex mcp list` failed")),
    }
}

/// Whether `child` exited within `timeout`, as [`crate::process::exited_within`] reports it.
#[cfg(unix)]
fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> Option<bool> {
    crate::process::exited_within(child.id(), timeout)
}

/// Whether `child` exited within `timeout`; `None` when it cannot be waited for.
#[cfg(not(unix))]
fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> Option<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Some(true),
            Ok(None) if Instant::now() >= deadline => return Some(false),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

/// The server names in `codex mcp list --json` output: an array of objects with a `name`.
pub(crate) fn parse_codex_list(out: &[u8]) -> Result<Vec<String>, LaunchError> {
    if out.len() as u64 > MAX_BYTES {
        return Err(refused("codex", "`codex mcp list` printed more than 1 MiB"));
    }
    let list: Value =
        serde_json::from_slice(out).map_err(|_| refused("codex", "`codex mcp list` did not print a JSON list"))?;
    let entries = list
        .as_array()
        .ok_or_else(|| refused("codex", "`codex mcp list` did not print a JSON list"))?;
    entries
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| refused("codex", "`codex mcp list` printed a server without a name"))
        })
        .collect()
}

/// The `-c` overrides that switch off each of the user's Codex servers. A name Codex's dotted
/// override path cannot carry, or one equal to the injected server's (whose settings the user's
/// entry would extend), refuses the launch.
pub(crate) fn codex_disable_args(users: &[String], injected: Option<&str>) -> Result<Vec<String>, LaunchError> {
    let mut args = Vec::with_capacity(users.len() * 2);
    for name in users {
        check_name("codex", name, injected)?;
        args.extend(["-c".to_owned(), format!("mcp_servers.{name}.enabled=false")]);
    }
    Ok(args)
}

fn check_name(harness: &'static str, name: &str, injected: Option<&str>) -> Result<(), LaunchError> {
    if injected == Some(name) {
        return Err(refused(
            harness,
            format!("the user configuration defines an MCP server named {name:?}, the injected server's name"),
        ));
    }
    let plain = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if plain {
        Ok(())
    } else {
        Err(refused(
            harness,
            "the user configuration defines an MCP server whose name is not [A-Za-z0-9_-]{1,64}",
        ))
    }
}

/// OpenCode's user configuration directory as the child will resolve it:
/// `$XDG_CONFIG_HOME/opencode`, else `$HOME/.config/opencode`, from the plan's environment. An
/// empty variable counts as unset and a relative one is taken against `cwd`, as OpenCode 2.3.2
/// does; with neither set the launch is refused, as the user's servers could not be found.
pub(crate) fn opencode_config_dir(env: &BTreeMap<String, OsString>, cwd: &Path) -> Result<PathBuf, LaunchError> {
    let set = |name: &str| {
        env.get(name)
            .filter(|value| !value.is_empty())
            .map(|value| cwd.join(value))
    };
    set("XDG_CONFIG_HOME")
        .or_else(|| set("HOME").map(|home| home.join(".config")))
        .map(|dir| dir.join("opencode"))
        .ok_or_else(|| {
            refused(
                "opencode",
                "neither XDG_CONFIG_HOME nor HOME is set, so the user configuration cannot be read",
            )
        })
}

/// What the launch overrides in the configuration OpenCode merges for it: the user's MCP servers,
/// each switched off, and the user's agents, each given the launch's permission rules.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenCodeUser {
    /// MCP server names, in the order first read.
    pub(crate) servers: Vec<String>,
    /// Agent names, from the `agent`, `mode` and `agents` keys and from agent Markdown files.
    pub(crate) agents: Vec<String>,
}

/// The servers and agents of every configuration OpenCode 2.3.2 merges beneath an inline one:
/// `opencode.json{,c}` and the agent Markdown files of `global` (its user directory), and of each
/// ancestor of `cwd` its `opencode.json{,c}` and the same in its `.opencode` directory. The
/// project-file check refuses those in every ancestor but the home directory, whose files OpenCode
/// loads when the working directory lies under it. A file that cannot be read or parsed, or a
/// name that OpenCode would substitute, refuses the launch rather than leaving it in effect.
/// Managed (system, MDM preferences) and organisation (`.well-known`) configurations are merged
/// after the inline one and are not read.
pub(crate) fn opencode_user(global: &Path, cwd: &Path) -> Result<OpenCodeUser, LaunchError> {
    let mut user = OpenCodeUser::default();
    read_opencode_dir(global, "the user configuration", &mut user)?;
    for dir in cwd.ancestors() {
        for file in OPENCODE_FILES {
            if let Some(document) = read_opencode_file(&dir.join(file), "an ancestor's", file)? {
                add_document(&document, file, &mut user)?;
            }
        }
        read_opencode_dir(&dir.join(".opencode"), "an ancestor's .opencode", &mut user)?;
    }
    Ok(user)
}

/// The configuration files and agent Markdown files of one OpenCode configuration directory.
fn read_opencode_dir(dir: &Path, label: &str, user: &mut OpenCodeUser) -> Result<(), LaunchError> {
    for file in OPENCODE_FILES {
        if let Some(document) = read_opencode_file(&dir.join(file), label, file)? {
            add_document(&document, file, user)?;
        }
    }
    let mut files = 0;
    for (sub, recursive) in AGENT_DIRS {
        markdown_agents(&dir.join(sub), "", recursive, 0, &mut files, user)?;
    }
    Ok(())
}

/// The parsed file at `path`, or `None` when there is none.
fn read_opencode_file(path: &Path, label: &str, file: &str) -> Result<Option<Value>, LaunchError> {
    let text = match std::fs::metadata(path) {
        Err(error) if absent(&error) => return Ok(None),
        Err(_) => return Err(refused("opencode", format!("cannot read {label} {file}"))),
        Ok(meta) if meta.len() > MAX_BYTES => {
            return Err(refused("opencode", format!("{label} {file} is over 1 MiB")));
        }
        Ok(_) => {
            std::fs::read_to_string(path).map_err(|_| refused("opencode", format!("cannot read {label} {file}")))?
        }
    };
    serde_json::from_str(&strip_jsonc(&text))
        .map(Some)
        .map_err(|_| refused("opencode", format!("cannot parse {label} {file}")))
}

/// Whether `error` says there is nothing at the path: it, or a directory on the way to it, is
/// missing, or that directory is a file (a `.opencode` file in an ancestor holds no configuration).
fn absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Adds the server and agent names of one configuration document, as OpenCode 2.3.2 normalizes
/// it: under `mcp`, an entry with a `type` of `local` or `remote` is a server whatever its key,
/// `servers` otherwise holds the servers by name (the canonical form), `timeout` is a setting,
/// and an entry naming only `enabled` is dropped; agents are the keys of `agent`, `mode` and
/// `agents`.
fn add_document(document: &Value, file: &str, user: &mut OpenCodeUser) -> Result<(), LaunchError> {
    let not_object = |key: &str| refused("opencode", format!("`{key}` in {file} is not an object"));
    match &document["mcp"] {
        Value::Null => {}
        Value::Object(entries) => {
            for (key, entry) in entries {
                let server = matches!(entry["type"].as_str(), Some("local" | "remote"));
                let enabled_only = entry.is_object() && entry.get("type").is_none() && entry["enabled"].is_boolean();
                if enabled_only {
                    continue;
                }
                if key == "servers" && !server {
                    let servers = entry.as_object().ok_or_else(|| not_object("mcp.servers"))?;
                    for name in servers.keys() {
                        add_name(&mut user.servers, name)?;
                    }
                } else if key != "timeout" || server {
                    add_name(&mut user.servers, key)?;
                }
            }
        }
        _ => return Err(not_object("mcp")),
    }
    for key in ["agent", "mode", "agents"] {
        match &document[key] {
            Value::Null => {}
            Value::Object(agents) => {
                for name in agents.keys() {
                    add_name(&mut user.agents, name)?;
                }
            }
            _ => return Err(not_object(key)),
        }
    }
    Ok(())
}

/// Adds `name` once. A name holding `{env:` or `{file:` is refused: OpenCode substitutes those
/// in the file's text before parsing it, so the name it reads is not this one, and the same text
/// in the inline configuration would be substituted too.
fn add_name(names: &mut Vec<String>, name: &str) -> Result<(), LaunchError> {
    if name.contains("{env:") || name.contains("{file:") {
        return Err(refused(
            "opencode",
            "the user configuration names a server or an agent through `{env:` or `{file:`",
        ));
    }
    if !names.iter().any(|known| known == name) {
        names.push(name.to_owned());
    }
    Ok(())
}

/// The agents of the Markdown files under `dir` (`prefix` is their path below it so far), as
/// OpenCode 2.3.2 names them: the path below `agent/`, `agents/`, `mode/` or `modes/` without
/// `.md`. `recursive` is false for the mode directories, whose subdirectories OpenCode skips. A
/// directory that cannot be listed, a tree deeper than [`MAX_AGENT_DEPTH`] or more than
/// [`MAX_AGENT_FILES`] entries refuses the launch.
fn markdown_agents(
    dir: &Path,
    prefix: &str,
    recursive: bool,
    depth: usize,
    files: &mut usize,
    user: &mut OpenCodeUser,
) -> Result<(), LaunchError> {
    let too_many = || refused("opencode", "an agent directory is too large or too deep to read");
    if depth > MAX_AGENT_DEPTH {
        return Err(too_many());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if prefix.is_empty() && absent(&error) => return Ok(()),
        Err(_) => return Err(refused("opencode", "cannot list an agent directory")),
    };
    for entry in entries {
        *files += 1;
        if *files > MAX_AGENT_FILES {
            return Err(too_many());
        }
        let entry = entry.map_err(|_| refused("opencode", "cannot list an agent directory"))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| refused("opencode", "an agent file's name is not UTF-8"))?;
        let path = entry.path();
        // OpenCode follows symbolic links; `metadata` does too.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if recursive {
                markdown_agents(&path, &format!("{prefix}{name}/"), true, depth + 1, files, user)?;
            }
        } else if let Some(stem) = name.strip_suffix(".md") {
            add_name(&mut user.agents, &format!("{prefix}{stem}"))?;
        }
    }
    Ok(())
}

/// OpenCode's inline configuration (`OPENCODE_CONFIG_CONTENT`): every permission `ask` but the
/// injected server's `allow` tools, the same rules for each of the user's agents, and each of
/// the user's servers switched off.
///
/// OpenCode 2.3.2 turns every configuration's `permission` into rules, appends them in the order
/// the configurations merge (the inline one after the user's and the project's) and lets the
/// last matching rule win, so the inline `*` overrides the user's own top-level rules. An agent's
/// own rules are appended after every configuration's top-level ones, so each of the user's
/// agents is given the inline rules too, which come after its own. OpenCode names an MCP tool's
/// permission `<server>_<tool>`; `*` sorts before every name this crate admits, so the sorted map
/// keeps it first. A server's inline entry replaces the user's whole, so it is a complete one.
pub(crate) fn opencode_content(
    allow_tools: &[String],
    injected: Option<&str>,
    user: &OpenCodeUser,
) -> Result<String, LaunchError> {
    let permission = match injected.filter(|_| !allow_tools.is_empty()) {
        None => json!("ask"),
        Some(server) => {
            let mut rules = Map::new();
            rules.insert("*".to_owned(), json!("ask"));
            for tool in allow_tools {
                rules.insert(format!("{server}_{tool}"), json!("allow"));
            }
            Value::Object(rules)
        }
    };
    let mut content = Map::new();
    if !user.agents.is_empty() {
        let agents = user
            .agents
            .iter()
            .map(|name| (name.clone(), json!({ "permission": permission.clone() })))
            .collect();
        content.insert("agent".to_owned(), Value::Object(agents));
    }
    content.insert("permission".to_owned(), permission);
    if !user.servers.is_empty() {
        let mut servers = Map::new();
        for name in &user.servers {
            if injected == Some(name.as_str()) {
                return Err(refused(
                    "opencode",
                    format!("the user configuration defines an MCP server named {name:?}, the injected server's name"),
                ));
            }
            servers.insert(
                name.clone(),
                json!({"type": "local", "command": [NEVER_RUN], "enabled": false}),
            );
        }
        content.insert("mcp".to_owned(), Value::Object(servers));
    }
    Ok(Value::Object(content).to_string())
}

/// `text` with JSONC's comments and trailing commas removed, outside strings.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut pending_comma = false;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                while !text.is_char_boundary(i) {
                    i += 1;
                }
                flush_comma(&mut out, &mut pending_comma);
                out.push_str(&text[start..i]);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                out.push(' ');
                continue;
            }
            b',' => {
                flush_comma(&mut out, &mut pending_comma);
                pending_comma = true;
            }
            b']' | b'}' => {
                pending_comma = false;
                out.push(b as char);
            }
            _ if b.is_ascii_whitespace() => out.push(b as char),
            _ => {
                flush_comma(&mut out, &mut pending_comma);
                let mut end = i + 1;
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                out.push_str(&text[i..end]);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    flush_comma(&mut out, &mut pending_comma);
    out
}

/// Writes a comma held back until the next token shows it is not trailing.
fn flush_comma(out: &mut String, pending: &mut bool) {
    if std::mem::take(pending) {
        out.push(',');
    }
}

#[cfg(test)]
#[path = "user_mcp_tests.rs"]
mod tests;
