// The model and effort flag names (Claude Code `--model`/`--effort`, Grok `--model`/`--reasoning-effort`) are taken from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/acp.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! The checks a launch's free-form parts pass before any adapter uses them: the request's
//! model and effort, the environment additions and the arguments the adapter itself built.

use std::path::Path;

use crate::error::LaunchError;
use crate::harness::{AcpFlavor, HarnessKind};

/// The longest model or effort value, and the longest environment variable name.
const MAX_VALUE_BYTES: usize = 128;

/// What a model or effort value has to look like.
#[derive(Clone, Copy)]
enum Value {
    /// A model name: `[A-Za-z0-9._:/[]-]`, not starting with `-`.
    Model,
    /// A reasoning effort: `[a-z]`.
    Effort,
}

impl Value {
    fn accepts(self, value: &str) -> bool {
        let allowed = |byte: u8| match self {
            Self::Model => {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'[' | b']' | b'-')
            }
            Self::Effort => byte.is_ascii_lowercase(),
        };
        (1..=MAX_VALUE_BYTES).contains(&value.len()) && !value.starts_with('-') && value.bytes().all(allowed)
    }
}

/// How `kind` is told a model and an effort at launch, or `None` where this crate knows no
/// way. The adapter builds these arguments itself; nothing of the request reaches the
/// command line but the checked value.
fn model_flags(kind: HarnessKind, value: Value) -> Option<fn(&str) -> Vec<String>> {
    match (kind, value) {
        // monocode `provider.rs` passes both to Claude Code.
        (HarnessKind::ClaudeCode, Value::Model) => Some(|model| vec!["--model".to_owned(), model.to_owned()]),
        (HarnessKind::ClaudeCode, Value::Effort) => Some(|effort| vec!["--effort".to_owned(), effort.to_owned()]),
        // The grammar admits no `"`, so the TOML string needs no escaping.
        (HarnessKind::Codex, Value::Model) => Some(|model| vec!["-c".to_owned(), format!("model=\"{model}\"")]),
        (HarnessKind::Codex, Value::Effort) => {
            Some(|effort| vec!["-c".to_owned(), format!("model_reasoning_effort=\"{effort}\"")])
        }
        // monocode `provider/acp.rs` passes these to Grok's `agent` subcommand.
        (HarnessKind::Acp(AcpFlavor::Grok), Value::Model) => Some(|model| vec!["--model".to_owned(), model.to_owned()]),
        (HarnessKind::Acp(AcpFlavor::Grok), Value::Effort) => {
            Some(|effort| vec!["--reasoning-effort".to_owned(), effort.to_owned()])
        }
        // The other ACP agents select a model through the protocol, which this crate does not
        // drive, and a generic agent's flags are unknown.
        (HarnessKind::Acp(_), _) => None,
    }
}

/// The longest part of a refused value an error repeats.
const SHOWN_BYTES: usize = 64;

/// `value` cut to its first [`SHOWN_BYTES`], so an error does not echo a request's value whole.
fn shown(value: &str) -> String {
    if value.len() <= SHOWN_BYTES {
        return value.to_owned();
    }
    let mut end = SHOWN_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// The arguments that select `model` and `effort` for `kind`. A value outside its grammar is
/// refused with [`LaunchError::ArgumentNotAllowed`], and a value for a harness that takes
/// none with [`LaunchError::Unsupported`].
pub(crate) fn model_args(
    kind: HarnessKind,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<String>, LaunchError> {
    let mut args = Vec::new();
    for (value, grammar, feature) in [
        (model, Value::Model, "choosing a model at launch"),
        (effort, Value::Effort, "choosing a reasoning effort at launch"),
    ] {
        let Some(value) = value else { continue };
        let flags = model_flags(kind, grammar).ok_or(LaunchError::Unsupported {
            harness: kind.id(),
            feature,
        })?;
        if !grammar.accepts(value) {
            return Err(LaunchError::ArgumentNotAllowed {
                harness: kind.id(),
                argument: shown(value),
            });
        }
        args.extend(flags(value));
    }
    Ok(args)
}

/// Fragments that switch a harness's permission checks off. Checked on the adapter's own
/// argument list as a second line of defence: a request's model and effort cannot carry
/// one, as their grammar admits no leading `-` and the flag is the adapter's.
const BYPASS_FRAGMENTS: &[&str] = &[
    "dangerously-skip-permissions",
    "dangerously-bypass-approvals-and-sandbox",
    "bypassPermissions",
    "danger-full-access",
    "dangerFullAccess",
    "--always-approve",
    "--yolo",
    "yoloMode",
];

/// Refuses an argument list carrying a bypass fragment unless the launch really runs in an
/// authorized bypass mode.
pub(crate) fn guard_bypass(args: &[String], bypass: bool) -> Result<(), LaunchError> {
    if bypass {
        return Ok(());
    }
    match args
        .iter()
        .find(|arg| BYPASS_FRAGMENTS.iter().any(|fragment| arg.contains(fragment)))
    {
        Some(arg) => Err(LaunchError::BypassArgument(arg.clone())),
        None => Ok(()),
    }
}

/// Name suffixes an environment addition may carry: credentials of a model provider, which a
/// harness reads from its environment. An endpoint (`*_BASE_URL`) is not among them: it would
/// let whoever writes the spec choose where the harness sends its credentials.
const ENV_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN"];

/// Further names an addition may carry: provider settings that are neither a credential nor
/// a location of code or configuration.
const ENV_NAMES: &[&str] = &[
    "AWS_REGION",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
];

/// Prefixes no addition may carry, even when its suffix matches: basemind's own variables,
/// the dynamic loaders' and the language runtimes' code-loading switches.
const ENV_DENIED_PREFIXES: &[&str] = &[
    "BASEMIND_",
    "LD_",
    "DYLD_",
    "NODE_",
    "NPM_",
    "PYTHON",
    "RUBY",
    "PERL",
    "JAVA_",
];

/// Names no addition may carry: the harnesses' configuration directories and files, which
/// would hand the child a configuration of the caller's choosing.
const ENV_DENIED_NAMES: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "GEMINI_CLI_HOME",
    "CURSOR_CONFIG_DIR",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_CONFIG_CONTENT",
    "HERMES_HOME",
    "GROK_HOME",
];

/// Checks the name of an environment addition, of the spec or of its MCP server. It must be
/// `[A-Z][A-Z0-9_]*`, end in a provider suffix or be on [`ENV_NAMES`], and be neither inherited
/// through [`ENV_ALLOWLIST`](crate::ENV_ALLOWLIST) nor on a denied prefix or name.
pub(crate) fn check_env_name(name: &str) -> Result<(), LaunchError> {
    let grammar = name.len() <= MAX_VALUE_BYTES
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    let inherited = crate::spec::ENV_ALLOWLIST
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name));
    let denied = ENV_DENIED_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || ENV_DENIED_NAMES.contains(&name);
    let allowed = ENV_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) || ENV_NAMES.contains(&name);
    if grammar && !inherited && !denied && allowed {
        Ok(())
    } else {
        Err(LaunchError::EnvNotAllowed(name.to_owned()))
    }
}

/// Project files through which `kind` loads permissions, hooks, MCP servers or plugins from
/// the directory it works in or an ancestor of it, each with the entries of it that are
/// allowed. Claude Code is absent because its adapter switches those sources off by flag;
/// Grok, Hermes and a generic ACP agent are absent because their project files are not known
/// here. Cursor's directory is refused whole but for `rules`, as its list of files that
/// configure the agent (`cli.json`, `mcp.json`, `hooks.json`, ...) is not known to be complete.
fn project_config(kind: HarnessKind) -> &'static [(&'static str, &'static [&'static str])] {
    match kind {
        HarnessKind::Codex => &[(".codex", &[])],
        HarnessKind::Acp(AcpFlavor::Gemini) => &[(".gemini", &[])],
        HarnessKind::Acp(AcpFlavor::Cursor) => &[(".cursor", &["rules"])],
        HarnessKind::Acp(AcpFlavor::OpenCode) => &[("opencode.json", &[]), ("opencode.jsonc", &[]), (".opencode", &[])],
        HarnessKind::ClaudeCode | HarnessKind::Acp(_) => &[],
    }
}

/// Whether `kind` may load a `.env` file (`.env`, `.env.local`, ...) from the directory it
/// works in or an ancestor, where a variable such as `GOOGLE_GEMINI_BASE_URL` or `HTTPS_PROXY`
/// would redirect its requests and credentials. Gemini CLI reads the first `.env` above its
/// working directory (from its documentation; not run here). Claude Code 2.1.326, Codex 0.169
/// and OpenCode 2.3.0 were seen not to read one there; Cursor, Grok, Hermes and a generic agent
/// were not checked, so they are refused too.
fn loads_dotenv(kind: HarnessKind) -> bool {
    matches!(
        kind,
        HarnessKind::Acp(
            AcpFlavor::Gemini | AcpFlavor::Cursor | AcpFlavor::Grok | AcpFlavor::Hermes | AcpFlavor::Generic
        )
    )
}

/// The first `.env` file in `dir`, or `dir/.env` itself when the directory cannot be listed.
fn dotenv(dir: &Path) -> Option<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return loadable(&dir.join(".env"), &[]);
    };
    entries
        .flatten()
        .find(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with(".env")))
        .map(|entry| entry.path())
}

/// The first thing at `path` the harness may load: `path` itself, or, for a directory with
/// `allowed` entries, its first entry not among them. Anything that cannot be looked at counts
/// as present, so an unreadable directory refuses the launch rather than passing it.
fn loadable(path: &Path, allowed: &[&str]) -> Option<std::path::PathBuf> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(path.to_owned()),
    };
    if allowed.is_empty() || !metadata.is_dir() {
        return Some(path.to_owned());
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Some(path.to_owned());
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return Some(path.to_owned());
        };
        if !allowed.iter().any(|name| entry.file_name() == *name) {
            return Some(entry.path());
        }
    }
    None
}

/// Refuses a launch whose working directory, or an ancestor of it, holds a project file that
/// `kind` would load: a file the launched agent (or anyone else) wrote there could approve
/// tools, add MCP servers or run hooks behind the approval policy's back. `home` is skipped,
/// because the same names there are the user's own configuration.
///
/// The check is a snapshot taken when the launch is planned: a file written between the plan
/// and the harness reading its configuration, or during the session, is not seen.
pub(crate) fn check_project_config(kind: HarnessKind, cwd: &Path, home: Option<&Path>) -> Result<(), LaunchError> {
    let home = home.and_then(|home| home.canonicalize().ok());
    for dir in cwd.ancestors().filter(|dir| Some(*dir) != home.as_deref()) {
        let files = project_config(kind)
            .iter()
            .find_map(|(name, allowed)| loadable(&dir.join(name), allowed));
        if let Some(path) = files.or_else(|| loads_dotenv(kind).then(|| dotenv(dir)).flatten()) {
            return Err(LaunchError::ProjectConfig {
                harness: kind.id(),
                path,
            });
        }
    }
    Ok(())
}

/// Directories under the home directory that hold a harness's configuration, sessions or
/// credentials: a root that is, holds or lies inside one would put them where the harness lets
/// the agent work.
const HARNESS_HOME_DIRS: &[&str] = &[
    ".claude",
    ".codex",
    ".gemini",
    ".cursor",
    ".grok",
    ".hermes",
    ".config/opencode",
    ".local/share/opencode",
    ".local/share/basemind",
    "Library/Application Support/basemind",
];

/// Refuses a root (canonical) that contains `HOME`, `XDG_CONFIG_HOME` or `XDG_DATA_HOME`, or
/// that is, contains or lies inside a harness's directory ([`HARNESS_HOME_DIRS`]) or basemind's
/// `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`. `env` reads the launcher's environment.
pub(crate) fn check_root(root: &Path, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<(), LaunchError> {
    let resolved = |path: std::path::PathBuf| path.canonicalize().unwrap_or(path);
    let var = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
    };
    let home = var("HOME");
    let umbrellas = [home.clone(), var("XDG_CONFIG_HOME"), var("XDG_DATA_HOME")];
    let own = home
        .iter()
        .flat_map(|home| HARNESS_HOME_DIRS.iter().map(move |dir| home.join(dir)))
        .chain(var("BASEMIND_DATA_HOME"))
        .chain(var("BASEMIND_COMMS_DIR"));
    let refused = |dir: std::path::PathBuf| LaunchError::RootNotAllowed {
        root: root.to_owned(),
        dir,
    };
    for dir in umbrellas.into_iter().flatten().map(resolved) {
        if dir.starts_with(root) {
            return Err(refused(dir));
        }
    }
    for dir in own.map(resolved) {
        if dir.starts_with(root) || root.starts_with(&dir) {
            return Err(refused(dir));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
