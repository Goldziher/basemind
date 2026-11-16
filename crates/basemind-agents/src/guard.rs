//! The checks a launch spec's free-form parts pass before any adapter uses them: the caller's
//! extra arguments, the environment additions and the arguments the adapter itself built.

use crate::error::LaunchError;
use crate::harness::{AcpFlavor, HarnessKind};

/// The longest value an allow-listed flag may carry.
const MAX_VALUE_BYTES: usize = 128;

/// What an allow-listed flag's value has to look like.
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

/// The flags a caller may pass to `kind`, with their values' grammar. Everything else is
/// refused: a harness's flags can widen its permissions, add MCP servers or settings, or name
/// directories, and no list of dangerous flags stays complete as the harnesses evolve.
fn allowed_flags(kind: HarnessKind) -> &'static [(&'static str, Value)] {
    match kind {
        // `--model` and `--effort`: monocode `provider.rs` passes both to Claude Code.
        HarnessKind::ClaudeCode => &[("--model", Value::Model), ("--effort", Value::Effort)],
        // monocode `provider/acp.rs` passes these to Grok's `agent` subcommand.
        HarnessKind::Acp(AcpFlavor::Grok) => &[("--model", Value::Model), ("--reasoning-effort", Value::Effort)],
        // Codex takes its model through `-c`, checked by `codex_config`.
        HarnessKind::Codex => &[],
        // The other ACP agents select a model through the protocol, which this crate does not
        // drive, and a generic agent's flags are unknown.
        HarnessKind::Acp(_) => &[],
    }
}

/// The Codex configuration keys a caller may set with `-c key="value"`.
const CODEX_KEYS: &[(&str, Value)] = &[("model", Value::Model), ("model_reasoning_effort", Value::Effort)];

/// Checks the caller's extra arguments for `kind` against its allow-list. Each allowed flag
/// may appear once, as `--flag value` or `--flag=value`; positionals and any other flag are
/// refused. A bypass opt-in does not widen the list.
pub(crate) fn check_extra_args(kind: HarnessKind, args: &[String]) -> Result<(), LaunchError> {
    let refuse = |argument: &str| LaunchError::ArgumentNotAllowed {
        harness: kind.id(),
        argument: argument.to_owned(),
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        let mut take_value = || match inline {
            Some(value) => Some(value),
            None => args.next().map(String::as_str),
        };
        if kind == HarnessKind::Codex && matches!(flag, "-c" | "--config") {
            let value = take_value().ok_or_else(|| refuse(arg))?;
            let key = codex_config(value).ok_or_else(|| refuse(value))?;
            if seen.contains(&key) {
                return Err(refuse(value));
            }
            seen.push(key);
            continue;
        }
        let Some((name, grammar)) = allowed_flags(kind).iter().find(|(name, _)| *name == flag) else {
            return Err(refuse(arg));
        };
        let value = take_value().ok_or_else(|| refuse(arg))?;
        if seen.contains(name) || !grammar.accepts(value) {
            return Err(refuse(arg));
        }
        seen.push(name);
    }
    Ok(())
}

/// The key of an allowed Codex `-c key="value"` override, or `None` when it is not one.
fn codex_config(setting: &str) -> Option<&'static str> {
    let (key, value) = setting.split_once('=')?;
    let (key, grammar) = CODEX_KEYS.iter().find(|(name, _)| *name == key)?;
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    grammar.accepts(value).then_some(*key)
}

/// Fragments that switch a harness's permission checks off. Checked on the adapter's own
/// argument list as a second line of defence: the caller's arguments never get this far
/// unless the allow-list accepted them.
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

/// Name suffixes an environment addition may carry: credentials and endpoints of a model
/// provider, which a harness reads from its environment.
const ENV_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_BASE_URL"];

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

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
