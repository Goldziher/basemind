//! What to launch: the spec a caller builds and the checks every adapter runs on it first.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::error::LaunchError;
use crate::event::ApprovalRequest;
use crate::guard::{check_env_name, check_extra_args, check_project_config, guard_bypass};
use crate::harness::{AcpFlavor, Harness, HarnessKind};
use crate::identity::AgentIdentity;
use crate::session::CommandPlan;

/// How much the launched agent may do before it has to ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionMode {
    /// The harness asks before it writes or runs anything; each request goes to the policy.
    #[default]
    Supervised,
    /// The harness plans and reads only.
    ReadOnly,
    /// File edits inside the working directory proceed without a request.
    AcceptEdits,
    /// The harness skips its permission checks. Refused without a matching [`BypassOptIn`].
    Bypass,
}

/// The explicit consent a permission bypass needs: it names the harness and that harness's
/// own word for the mode, so a spec written for one harness cannot unlock another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BypassOptIn {
    /// The harness the consent is for.
    pub harness: HarnessKind,
    /// The mode name, equal to [`HarnessKind::bypass_mode`] when the harness has a fixed one.
    pub mode: String,
}

/// The answer of an [`ApprovalPolicy`] to one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// Let the harness proceed, this once.
    Allow,
    /// Refuse.
    Deny,
    /// Leave the request pending until [`Session::approve`](crate::Session::approve) or
    /// [`Session::deny`](crate::Session::deny) answers it.
    Ask,
}

/// Decides approval requests. A spec without a policy denies every one of them.
pub trait ApprovalPolicy: Send + Sync {
    /// The verdict for `request`.
    fn decide(&self, request: &ApprovalRequest) -> ApprovalVerdict;
}

impl<F> ApprovalPolicy for F
where
    F: Fn(&ApprovalRequest) -> ApprovalVerdict + Send + Sync,
{
    fn decide(&self, request: &ApprovalRequest) -> ApprovalVerdict {
        self(request)
    }
}

/// When a session is ended from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The longest the harness process may live. Its process group is killed at the deadline.
    pub wall_time: Duration,
    /// How many prompts (the brief and each follow-up) the session runs. A turn the harness
    /// starts beyond that count kills the process group.
    pub max_turns: u32,
    /// How long a stopped harness may take to wind down before its process group is killed.
    pub stop_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_time: Duration::from_secs(30 * 60),
            max_turns: 1,
            stop_grace: Duration::from_secs(5),
        }
    }
}

/// An MCP stdio server the launched harness should start, normally `basemind serve`.
///
/// It is data: each adapter turns it into that harness's own form (Claude Code's
/// `--mcp-config`, Codex's `-c mcp_servers.<name>.*`, ACP's `mcpServers`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerSpec {
    /// The server name, `[A-Za-z0-9_-]{1,64}`.
    pub name: String,
    /// The server executable.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables the server needs besides the agent identity, under the same name rule as
    /// [`LaunchSpec::env`]. They are exported to the harness as well, because Claude Code and
    /// Codex hand a server variables by inheritance.
    pub env: BTreeMap<String, String>,
    /// The tools of this server the harness may call without an approval request, by their
    /// MCP name (`[A-Za-z0-9_-]{1,64}`). Empty by default: every call is then a request the
    /// approval policy decides, and without a policy it is denied. Only Claude Code can
    /// pre-approve single tools; for Codex and ACP agents a non-empty list fails the launch
    /// with [`LaunchError::Unsupported`].
    pub allow_tools: Vec<String>,
}

impl McpServerSpec {
    /// `<executable> serve` under the name `basemind`, with no tool pre-approved.
    #[must_use]
    pub fn basemind_serve(executable: impl Into<PathBuf>) -> Self {
        Self {
            name: "basemind".to_owned(),
            command: executable.into(),
            args: vec!["serve".to_owned()],
            env: BTreeMap::new(),
            allow_tools: Vec::new(),
        }
    }
}

/// Everything one launch needs.
#[derive(Clone)]
#[non_exhaustive]
pub struct LaunchSpec {
    /// The harness to start.
    pub harness: Harness,
    /// The directory the agent is confined to.
    pub root: PathBuf,
    /// The agent's working directory, inside `root`. `None` means `root`.
    pub working_dir: Option<PathBuf>,
    /// The brief: the first prompt the agent receives.
    pub brief: String,
    /// The identity exported to the agent.
    pub identity: AgentIdentity,
    /// Variables added to the allow-listed environment. Nothing else of the parent's
    /// environment reaches the child.
    ///
    /// A name must be `[A-Z][A-Z0-9_]*` and end in `_API_KEY`, `_TOKEN` or `_BASE_URL`, or be
    /// one of a few provider settings (`AWS_REGION`, `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`). Names on
    /// [`ENV_ALLOWLIST`], `BASEMIND_*`, loader and runtime switches (`LD_*`, `DYLD_*`,
    /// `NODE_*`, `PYTHON*`, ...) and the harnesses' configuration directories are refused
    /// with [`LaunchError::EnvNotAllowed`], so an addition cannot redirect the child's
    /// configuration, code loading, executable search or basemind daemon.
    pub env: BTreeMap<String, String>,
    /// The MCP server to inject, if any.
    pub mcp_server: Option<McpServerSpec>,
    /// Wall-time and turn limits.
    pub limits: Limits,
    /// How much the agent may do unasked.
    pub permission_mode: PermissionMode,
    /// Consent for [`PermissionMode::Bypass`] and for bypass arguments in [`Harness::args`].
    pub bypass: Option<BypassOptIn>,
    /// Who answers approval requests. `None` denies all of them.
    pub approval_policy: Option<Arc<dyn ApprovalPolicy>>,
}

impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("harness", &self.harness)
            .field("root", &self.root)
            .field("working_dir", &self.working_dir)
            .field("identity", &self.identity)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("mcp_server", &self.mcp_server.as_ref().map(|server| &server.name))
            .field("limits", &self.limits)
            .field("permission_mode", &self.permission_mode)
            .field("bypass", &self.bypass)
            .field("approval_policy", &self.approval_policy.is_some())
            .finish_non_exhaustive()
    }
}

impl LaunchSpec {
    /// A supervised single-turn launch of `harness` in `root` with every default.
    #[must_use]
    pub fn new(harness: Harness, root: impl Into<PathBuf>, brief: impl Into<String>, identity: AgentIdentity) -> Self {
        Self {
            harness,
            root: root.into(),
            working_dir: None,
            brief: brief.into(),
            identity,
            env: BTreeMap::new(),
            mcp_server: None,
            limits: Limits::default(),
            permission_mode: PermissionMode::default(),
            bypass: None,
            approval_policy: None,
        }
    }

    /// Checks the spec and resolves what the adapters need from it.
    pub(crate) fn prepare(&self) -> Result<Prepared, LaunchError> {
        if self.limits.wall_time.is_zero() {
            return Err(LaunchError::InvalidLimits("wall_time is zero"));
        }
        if self.limits.max_turns == 0 {
            return Err(LaunchError::InvalidLimits("max_turns is zero"));
        }
        for name in self
            .env
            .keys()
            .chain(self.mcp_server.iter().flat_map(|server| server.env.keys()))
        {
            check_env_name(name)?;
        }
        if let Some(server) = &self.mcp_server {
            let valid = mcp_name(&server.name);
            if !valid {
                return Err(LaunchError::InvalidMcpServerName(server.name.clone()));
            }
            for tool in &server.allow_tools {
                if !mcp_name(tool) {
                    return Err(LaunchError::InvalidMcpToolName(tool.clone()));
                }
            }
        }
        let kind = self.harness.kind;
        let authorized = self.bypass_mode()?;
        if self.permission_mode == PermissionMode::Bypass && authorized.is_none() {
            return Err(LaunchError::BypassNotAuthorized {
                harness: kind.id(),
                mode: kind.bypass_mode().unwrap_or("<the session mode id>").to_owned(),
            });
        }
        check_extra_args(kind, &self.harness.args)?;
        let cwd = confine(&self.root, self.working_dir.as_deref())?;
        check_project_config(kind, &cwd, std::env::var_os("HOME").as_deref().map(Path::new))?;
        Ok(Prepared {
            cwd,
            bypass_mode: authorized,
            extra_args: self.harness.args.clone(),
        })
    }

    /// The bypass mode the opt-in authorizes for this harness, or `None` without a valid one.
    fn bypass_mode(&self) -> Result<Option<String>, LaunchError> {
        let kind = self.harness.kind;
        let Some(opt_in) = self.bypass.as_ref().filter(|opt_in| opt_in.harness == kind) else {
            return Ok(None);
        };
        match kind.bypass_mode() {
            Some(mode) if opt_in.mode == mode => Ok(Some(opt_in.mode.clone())),
            Some(_) => Ok(None),
            None if kind == HarnessKind::Acp(AcpFlavor::Generic) && !opt_in.mode.is_empty() => {
                Ok(Some(opt_in.mode.clone()))
            }
            None => Err(LaunchError::Unsupported {
                harness: kind.id(),
                feature: "a permission bypass",
            }),
        }
    }

    /// The plan for the adapter's final `args`, which include the checked extra arguments
    /// where that harness takes them, after the bypass guard.
    pub(crate) fn plan(
        &self,
        prepared: &Prepared,
        args: Vec<String>,
        extra_env: &[(&str, &str)],
    ) -> Result<CommandPlan, LaunchError> {
        guard_bypass(&args, prepared.bypass(self))?;
        let mut env = self.child_env(std::env::vars_os());
        for (name, value) in extra_env {
            env.entry((*name).to_owned()).or_insert_with(|| value.into());
        }
        Ok(CommandPlan {
            program: self.harness.executable.clone(),
            args,
            env,
            cwd: prepared.cwd.clone(),
        })
    }

    /// The names of the variables an injected MCP server must see: the identity, the server's
    /// own, and the store and comms locations when the child has them.
    pub(crate) fn mcp_env_names(&self) -> Vec<String> {
        let server_env = self.mcp_server.iter().flat_map(|server| server.env.keys());
        let wanted: Vec<&str> = [crate::identity::ENV_AGENT_ID, crate::identity::ENV_HOST]
            .into_iter()
            .chain(SHARED_STATE_ENV.iter().copied())
            .chain(server_env.map(String::as_str))
            .collect();
        self.child_env(std::env::vars_os())
            .into_keys()
            .filter(|name| wanted.contains(&name.as_str()))
            .collect()
    }

    /// The environment of the child: the allow-listed part of `parent`, then the additions,
    /// then the MCP server's variables, then the identity.
    pub(crate) fn child_env(
        &self,
        parent: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> BTreeMap<String, OsString> {
        let mut env = BTreeMap::new();
        for (name, value) in parent {
            if let Some(name) = name.to_str().filter(|name| ENV_ALLOWLIST.contains(name)) {
                env.insert(name.to_owned(), value);
            }
        }
        let server_env = self.mcp_server.iter().flat_map(|server| &server.env);
        for (name, value) in self.env.iter().chain(server_env) {
            env.insert(name.clone(), value.into());
        }
        for (name, value) in self.identity.env(self.harness.kind.id()) {
            env.insert(name.to_owned(), value.into());
        }
        env
    }
}

/// The parts of a checked spec the adapters build on.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    /// The canonical working directory, inside the canonical root.
    pub cwd: PathBuf,
    /// The authorized bypass mode, when the spec carries a valid opt-in.
    pub bypass_mode: Option<String>,
    /// The caller's extra arguments, checked against the harness's allow-list.
    pub extra_args: Vec<String>,
}

impl Prepared {
    /// Whether the launch runs with the harness's permission checks off.
    pub fn bypass(&self, spec: &LaunchSpec) -> bool {
        spec.permission_mode == PermissionMode::Bypass && self.bypass_mode.is_some()
    }
}

/// The variables a child inherits from its launcher. Everything else, credentials included,
/// reaches it only through [`LaunchSpec::env`].
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "SystemRoot",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PATHEXT",
    "TEMP",
    "TMP",
    "ComSpec",
    "BASEMIND_DATA_HOME",
    "BASEMIND_COMMS_DIR",
];

/// Where basemind keeps its store and its comms rendezvous. A launched agent has to see the
/// launcher's values, or its `basemind serve` talks to a different daemon.
const SHARED_STATE_ENV: &[&str] = &["BASEMIND_DATA_HOME", "BASEMIND_COMMS_DIR"];

/// Whether `name` can be written into every harness's configuration: `[A-Za-z0-9_-]{1,64}`.
fn mcp_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn confine(root: &Path, working_dir: Option<&Path>) -> Result<PathBuf, LaunchError> {
    let resolve = |path: &Path| {
        path.canonicalize().map_err(|source| LaunchError::Path {
            path: path.to_owned(),
            source,
        })
    };
    let root = resolve(root)?;
    let dir = match working_dir {
        Some(dir) if dir.is_absolute() => resolve(dir)?,
        Some(dir) => resolve(&root.join(dir))?,
        None => root.clone(),
    };
    if !dir.starts_with(&root) {
        return Err(LaunchError::WorkingDirOutsideRoot { dir, root });
    }
    Ok(dir)
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
