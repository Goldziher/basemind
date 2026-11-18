//! The user's own MCP servers, which Codex and OpenCode start from their user-level configuration
//! beside the injected one. Both harnesses merge a launch's configuration into the user's, so
//! nothing a launch passes removes a server: each has to be switched off by name. The names are
//! read at launch, from the harness itself (Codex) or from the configuration files (OpenCode).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::error::LaunchError;
use crate::session::CommandPlan;

/// Codex features that load MCP servers the user's `mcp_servers` table does not list: plugins
/// (whose servers `codex mcp list` leaves out once they are off) and the ChatGPT apps
/// connectors. Codex 0.169 takes them as `-c features.<name>=false`.
pub(crate) const CODEX_FEATURES_OFF: [&str; 4] = ["-c", "features.plugins=false", "-c", "features.apps=false"];

/// How long `codex mcp list` may take before the launch is refused.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// The most output `codex mcp list` or an OpenCode configuration file may have.
const MAX_BYTES: u64 = 1024 * 1024;
/// The command OpenCode is given for each server it switches off. OpenCode 2.3.2 dropped a
/// server entry that names `enabled` alone, so the entry is a whole one; it is never run.
const NEVER_RUN: &str = "/usr/bin/true";
/// The files OpenCode 2.3.2 reads from its user configuration directory, in its order.
const OPENCODE_FILES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

fn refused(harness: &'static str, reason: impl Into<String>) -> LaunchError {
    LaunchError::UserMcpServers {
        harness,
        reason: reason.into(),
    }
}

/// The user's Codex MCP servers, as `<codex> -c features.plugins=false -c features.apps=false mcp
/// list --json` reports them in the plan's environment and working directory: the merged
/// configuration Codex itself would start them from, managed configuration included.
pub(crate) fn codex_servers(plan: &CommandPlan) -> Result<Vec<String>, LaunchError> {
    let mut child = Command::new(&plan.program)
        .args(CODEX_FEATURES_OFF)
        .args(["mcp", "list", "--json"])
        .env_clear()
        .envs(&plan.env)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| LaunchError::Spawn {
            executable: plan.program.clone(),
            source,
        })?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            // Past the cap the rest is not read; the parse then fails.
            let _ = stdout.take(MAX_BYTES + 1).read_to_end(&mut out);
        }
        out
    });
    let deadline = Instant::now() + LIST_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader.join().unwrap_or_default();
    match status {
        Some(status) if status.success() => parse_codex_list(&out),
        Some(_) => Err(refused("codex", "`codex mcp list` failed")),
        None => Err(refused("codex", "`codex mcp list` did not finish in time")),
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

/// OpenCode's user configuration directory as the child will resolve it: `$XDG_CONFIG_HOME/opencode`,
/// else `$HOME/.config/opencode`, from the plan's environment.
pub(crate) fn opencode_config_dir(env: &BTreeMap<String, OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| env.get(name).map(PathBuf::from).filter(|path| path.is_absolute());
    absolute("XDG_CONFIG_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".config")))
        .map(|dir| dir.join("opencode"))
}

/// The `mcp` server names of OpenCode's user configuration files in `dir`. A file that cannot
/// be read or parsed refuses the launch rather than leaving its servers on.
pub(crate) fn opencode_servers(dir: &Path) -> Result<Vec<String>, LaunchError> {
    let mut names = Vec::new();
    for file in OPENCODE_FILES {
        let path = dir.join(file);
        let text = match std::fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                return Err(refused(
                    "opencode",
                    format!("cannot read the user configuration {file}"),
                ));
            }
            Ok(meta) if meta.len() > MAX_BYTES => {
                return Err(refused(
                    "opencode",
                    format!("the user configuration {file} is over 1 MiB"),
                ));
            }
            Ok(_) => std::fs::read_to_string(&path)
                .map_err(|_| refused("opencode", format!("cannot read the user configuration {file}")))?,
        };
        let document: Value = serde_json::from_str(&strip_jsonc(&text))
            .map_err(|_| refused("opencode", format!("cannot parse the user configuration {file}")))?;
        match &document["mcp"] {
            Value::Null => {}
            Value::Object(servers) => {
                for name in servers.keys() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            }
            _ => {
                return Err(refused(
                    "opencode",
                    format!("`mcp` in the user configuration {file} is not an object"),
                ));
            }
        }
    }
    Ok(names)
}

/// OpenCode's inline configuration (`OPENCODE_CONFIG_CONTENT`): every permission `ask` but the
/// injected server's `allow` tools, and each of the user's servers switched off.
///
/// OpenCode 2.3.2 names an MCP tool's permission `<server>_<tool>` and lets the last matching
/// rule win, in the object's order; `*` sorts before every name this crate admits, so the
/// sorted map keeps it first.
pub(crate) fn opencode_content(
    allow_tools: &[String],
    injected: Option<&str>,
    users: &[String],
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
    content.insert("permission".to_owned(), permission);
    if !users.is_empty() {
        let mut servers = Map::new();
        for name in users {
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
