// `PermissionMode` is adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/agent.rs, `AccessMode`) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! What to launch: the trusted [`LaunchConfig`], the model-facing [`LaunchRequest`], and the
//! checks every adapter runs on the two before it builds a command.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::error::LaunchError;
use crate::event::ApprovalRequest;
use crate::guard::{check_env_name, check_project_config, check_root, guard_bypass, model_args};
use crate::harness::{AcpFlavor, Harness, HarnessKind};
use crate::identity::AgentIdentity;
use crate::session::CommandPlan;

/// How much the launched agent may do before it has to ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionMode {
    /// The harness's own default mode: it asks before whatever its rules treat as needing
    /// approval (writes and commands, for most), and each request goes to the policy. What
    /// it does unasked, such as reading files, is up to the harness.
    #[default]
    Supervised,
    /// The harness's planning or read-only mode, where it has one; refused where it has none.
    ReadOnly,
    /// The harness's accept-edits mode: file edits proceed without a request. Refused where
    /// the harness has no such mode, and for Codex, whose nearest mode also runs commands
    /// unasked.
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

/// The explicit consent to run an ACP agent in a permission mode whose behaviour this crate
/// has not verified: what the agent does without asking is then up to the agent and its
/// user-level configuration. It names the harness and the mode, so consent for one does not
/// cover another. Claude Code and Codex never need it: their adapters set the mode
/// explicitly, and Claude Code's is checked against what it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnverifiedModeOptIn {
    /// The harness the consent is for.
    pub harness: HarnessKind,
    /// The permission mode the consent is for.
    pub mode: PermissionMode,
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

/// When a session is ended from outside. In a [`LaunchConfig`] these are the caps: a
/// [`LaunchRequest`] may ask for less wall time or fewer turns, never more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The longest the harness process may live. Its process group is killed at the deadline.
    pub wall_time: Duration,
    /// How many prompts (the brief and each follow-up) the session runs. A turn the harness
    /// starts beyond that count kills the process group.
    pub max_turns: u32,
    /// How long a harness told to [`Session::stop`](crate::Session::stop) may take to wind
    /// down, in all, before its process group is killed.
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
/// `--mcp-config`, Codex's `-c mcp_servers.<name>.*`, ACP's `mcpServers`). Its `Debug` output
/// names the environment variables without their values and counts the arguments.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    /// The server name, `[A-Za-z0-9_-]{1,64}`.
    pub name: String,
    /// The server executable: an absolute path, or a bare name looked up on the absolute
    /// entries of the launcher's `PATH`. One inside the root is refused.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables the server needs besides the agent identity, under the same name rule as
    /// [`LaunchConfig::env`]. They are exported to the harness as well, because Claude Code and
    /// Codex hand a server variables by inheritance.
    pub env: BTreeMap<String, String>,
    /// The tools of this server the harness may call without an approval request, by their
    /// MCP name (`[A-Za-z0-9_-]{1,64}`). Empty by default: every call is then a request the
    /// approval policy decides, and without a policy it is denied. Only Claude Code can
    /// pre-approve single tools; for Codex and ACP agents a non-empty list fails the launch
    /// with [`LaunchError::Unsupported`].
    pub allow_tools: Vec<String>,
}

impl std::fmt::Debug for McpServerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerSpec")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("arg_count", &self.args.len())
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("allow_tools", &self.allow_tools)
            .finish()
    }
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

/// The trusted half of a launch: what the operator's configuration, or the engine acting on
/// it, decides. Nothing here may come from a model, a brief or another agent: the executable
/// and the MCP server's command are run, the environment values pick the account the
/// harness works under, and the opt-ins switch safety checks off. A model-chosen part of a
/// launch goes in a [`LaunchRequest`], which cannot express any of these.
#[derive(Clone)]
#[non_exhaustive]
pub struct LaunchConfig {
    /// The harness to start.
    pub harness: Harness,
    /// The directory the working directory has to resolve inside. Only the starting
    /// directory is checked (at the plan and again in the child after it changes into it);
    /// the agent is not sandboxed to it, and a harness that is sent the path in its protocol
    /// (Codex, ACP) resolves it again itself.
    pub root: PathBuf,
    /// The identity exported to the agent.
    pub identity: AgentIdentity,
    /// Variables added to the allow-listed environment. Nothing else of the parent's
    /// environment reaches the child.
    ///
    /// A name must be `[A-Z][A-Z0-9_]*` and end in `_API_KEY` or `_TOKEN`, or be
    /// one of a few provider settings (`AWS_REGION`, `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`). Names on
    /// [`ENV_ALLOWLIST`], `BASEMIND_*`, loader and runtime switches (`LD_*`, `DYLD_*`,
    /// `NODE_*`, `PYTHON*`, ...) and the harnesses' configuration directories are refused
    /// with [`LaunchError::EnvNotAllowed`], so an addition cannot redirect the child's
    /// configuration, code loading, executable search or basemind daemon. The values are
    /// trusted: a credential and the project and region names choose which account the
    /// harness bills and where its requests go.
    pub env: BTreeMap<String, String>,
    /// The MCP server to inject, if any.
    pub mcp_server: Option<McpServerSpec>,
    /// The caps on wall time and turns, and the stop grace.
    pub limits: Limits,
    /// How much the agent may do unasked.
    pub permission_mode: PermissionMode,
    /// Consent for [`PermissionMode::Bypass`].
    pub bypass: Option<BypassOptIn>,
    /// Consent to an ACP agent's permission mode that was not verified to ask.
    pub unverified_mode: Option<UnverifiedModeOptIn>,
    /// Who answers approval requests. `None` denies all of them.
    pub approval_policy: Option<Arc<dyn ApprovalPolicy>>,
}

impl std::fmt::Debug for LaunchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchConfig")
            .field("harness", &self.harness)
            .field("root", &self.root)
            .field("identity", &self.identity)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("mcp_server", &self.mcp_server.as_ref().map(|server| &server.name))
            .field("limits", &self.limits)
            .field("permission_mode", &self.permission_mode)
            .field("bypass", &self.bypass)
            .field("unverified_mode", &self.unverified_mode)
            .field("approval_policy", &self.approval_policy.is_some())
            .finish_non_exhaustive()
    }
}

impl LaunchConfig {
    /// A supervised launch of `harness` in `root` with the default limits as caps, no
    /// environment additions, no MCP server and no approval policy.
    #[must_use]
    pub fn new(harness: Harness, root: impl Into<PathBuf>, identity: AgentIdentity) -> Self {
        Self {
            harness,
            root: root.into(),
            identity,
            env: BTreeMap::new(),
            mcp_server: None,
            limits: Limits::default(),
            permission_mode: PermissionMode::default(),
            bypass: None,
            unverified_mode: None,
            approval_policy: None,
        }
    }
}

/// The half of a launch a model may choose: what to do, where in the root, with which model,
/// for how long. Every field is checked against the [`LaunchConfig`] it is launched under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaunchRequest {
    /// The brief: the first prompt the agent receives, at most 1 MiB. One that starts with
    /// `/` is refused with [`LaunchError::SlashCommandPrompt`].
    pub brief: String,
    /// The working directory, relative to the config's root; `None` means the root. An
    /// absolute path is refused with [`LaunchError::WorkingDirNotRelative`], and one that
    /// resolves outside the root with [`LaunchError::WorkingDirOutsideRoot`].
    pub working_dir: Option<PathBuf>,
    /// The model, for a harness that takes one at launch (Claude Code, Codex, Grok):
    /// `[A-Za-z0-9._:/[]-]{1,128}`, not starting with `-`. Any other harness refuses it with
    /// [`LaunchError::Unsupported`].
    pub model: Option<String>,
    /// The reasoning effort, `[a-z]{1,128}`, for the same harnesses as `model`.
    pub effort: Option<String>,
    /// How many prompts the session may run, at most the config's [`Limits::max_turns`];
    /// a larger number is lowered to it. `None` takes the cap.
    pub max_turns: Option<u32>,
    /// How long the harness may run, at most the config's [`Limits::wall_time`]; a longer
    /// time is lowered to it. `None` takes the cap.
    pub wall_time: Option<Duration>,
}

impl LaunchRequest {
    /// A request for `brief` in the root, with the config's caps as its limits.
    #[must_use]
    pub fn new(brief: impl Into<String>) -> Self {
        Self {
            brief: brief.into(),
            ..Self::default()
        }
    }
}

/// One launch: a config and a request, merged after the request's limits were clamped to
/// the config's caps. Adapters build on this and never on the request alone.
#[derive(Clone)]
pub(crate) struct LaunchSpec {
    pub harness: Harness,
    pub root: PathBuf,
    pub working_dir: Option<PathBuf>,
    pub brief: String,
    pub identity: AgentIdentity,
    pub env: BTreeMap<String, String>,
    pub mcp_server: Option<McpServerSpec>,
    pub limits: Limits,
    pub permission_mode: PermissionMode,
    pub bypass: Option<BypassOptIn>,
    pub unverified_mode: Option<UnverifiedModeOptIn>,
    pub approval_policy: Option<Arc<dyn ApprovalPolicy>>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl LaunchSpec {
    /// Merges `request` into `config`: its limits are clamped to the config's caps, and a
    /// zero limit or an absolute working directory is refused.
    pub fn new(config: &LaunchConfig, request: &LaunchRequest) -> Result<Self, LaunchError> {
        crate::session::check_prompt(&request.brief)?;
        if request.max_turns == Some(0) {
            return Err(LaunchError::InvalidLimits("max_turns is zero"));
        }
        if request.wall_time.is_some_and(|wall_time| wall_time.is_zero()) {
            return Err(LaunchError::InvalidLimits("wall_time is zero"));
        }
        if let Some(dir) = request
            .working_dir
            .as_ref()
            .filter(|dir| dir.is_absolute() || dir.has_root())
        {
            return Err(LaunchError::WorkingDirNotRelative(dir.clone()));
        }
        let caps = config.limits;
        let limits = Limits {
            wall_time: request
                .wall_time
                .map_or(caps.wall_time, |asked| asked.min(caps.wall_time)),
            max_turns: request
                .max_turns
                .map_or(caps.max_turns, |asked| asked.min(caps.max_turns)),
            stop_grace: caps.stop_grace,
        };
        Ok(Self {
            harness: config.harness.clone(),
            root: config.root.clone(),
            working_dir: request.working_dir.clone(),
            brief: request.brief.clone(),
            identity: config.identity.clone(),
            env: config.env.clone(),
            mcp_server: config.mcp_server.clone(),
            limits,
            permission_mode: config.permission_mode,
            bypass: config.bypass.clone(),
            unverified_mode: config.unverified_mode,
            approval_policy: config.approval_policy.clone(),
            model: request.model.clone(),
            effort: request.effort.clone(),
        })
    }

    /// Checks the spec and resolves what the adapters need from it.
    pub fn prepare(&self) -> Result<Prepared, LaunchError> {
        if self.limits.wall_time.is_zero() {
            return Err(LaunchError::InvalidLimits("wall_time is zero"));
        }
        if self.limits.max_turns == 0 {
            return Err(LaunchError::InvalidLimits("max_turns is zero"));
        }
        // The supervisor adds these to the current instant; one that overflows would panic.
        let now = std::time::Instant::now();
        if now.checked_add(self.limits.wall_time.saturating_mul(2)).is_none() {
            return Err(LaunchError::InvalidLimits("wall_time is too long to schedule"));
        }
        if now.checked_add(self.limits.stop_grace.saturating_mul(2)).is_none() {
            return Err(LaunchError::InvalidLimits("stop_grace is too long to schedule"));
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
        let model_args = model_args(kind, self.model.as_deref(), self.effort.as_deref())?;
        let (root, cwd) = confine(&self.root, self.working_dir.as_deref())?;
        check_root(&root, &|name| std::env::var_os(name))?;
        check_project_config(kind, &cwd, std::env::var_os("HOME").as_deref().map(Path::new))?;
        crate::gitconfig::check_repository(&cwd)?;
        let search = std::env::var_os("PATH");
        let program = resolve_executable(&self.harness.executable, search.as_deref(), &root)?;
        let mcp_command = match &self.mcp_server {
            Some(server) => Some(resolve_executable(&server.command, search.as_deref(), &root)?),
            None => None,
        };
        // Every harness receives these paths inside JSON or TOML, which carry UTF-8 only.
        utf8(&cwd)?;
        if let Some(command) = &mcp_command {
            utf8(command)?;
        }
        Ok(Prepared {
            cwd,
            program,
            mcp_command,
            bypass_mode: authorized,
            model_args,
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

    /// The plan for the adapter's `args` followed by `server_args`. `args` are the adapter's
    /// own and the model arguments, and pass the bypass guard; `server_args` carry the
    /// config's MCP server (its command, arguments and tool names), which are trusted data
    /// the guard's fragments could match by accident, as in a path through a `yolo` directory.
    pub fn plan(
        &self,
        prepared: &Prepared,
        mut args: Vec<String>,
        server_args: Vec<String>,
        extra_env: &[(&str, &str)],
    ) -> Result<CommandPlan, LaunchError> {
        guard_bypass(&args, prepared.bypass(self))?;
        let server_arg_count = server_args.len();
        args.extend(server_args);
        let mut env = self.child_env(std::env::vars_os());
        for (name, value) in extra_env {
            env.entry((*name).to_owned()).or_insert_with(|| value.into());
        }
        for (name, value) in GIT_OVERRIDES {
            env.insert((*name).to_owned(), value.into());
        }
        Ok(CommandPlan {
            program: prepared.program.clone(),
            args,
            env,
            cwd: prepared.cwd.clone(),
            server_args: server_arg_count,
        })
    }

    /// The names of the variables an injected MCP server must see: the identity, the server's
    /// own, and the store and comms locations when the child has them.
    pub fn mcp_env_names(&self) -> Vec<String> {
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
    pub fn child_env(&self, parent: impl IntoIterator<Item = (OsString, OsString)>) -> BTreeMap<String, OsString> {
        let mut env = BTreeMap::new();
        for (name, value) in parent {
            let Some(name) = name.to_str().filter(|name| ENV_ALLOWLIST.contains(name)) else {
                continue;
            };
            let value = if name == "PATH" {
                absolute_search_path(&value, &self.root)
            } else {
                value
            };
            env.insert(name.to_owned(), value);
        }
        let server_env = self.mcp_server.iter().flat_map(|server| &server.env);
        for (name, value) in self.env.iter().chain(server_env) {
            env.insert(name.clone(), value.into());
        }
        for (name, value) in self.identity.env(self.harness.kind.id()) {
            env.insert(name.to_owned(), value.into());
        }
        // Without it, a bare program name a Windows harness runs (`node`, `git`, `rg`) is looked
        // up in the agent's working directory before `PATH`.
        #[cfg(windows)]
        env.insert("NoDefaultCurrentDirectoryInExePath".to_owned(), "1".into());
        env
    }
}

/// The parts of a checked spec the adapters build on.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    /// The canonical working directory, inside the canonical root.
    pub cwd: PathBuf,
    /// The harness executable, absolute and outside the root.
    pub program: PathBuf,
    /// The MCP server's command, absolute and outside the root.
    pub mcp_command: Option<PathBuf>,
    /// The authorized bypass mode, when the spec carries a valid opt-in.
    pub bypass_mode: Option<String>,
    /// The harness's arguments for the request's model and effort.
    pub model_args: Vec<String>,
}

impl Prepared {
    /// Whether the launch runs with the harness's permission checks off.
    pub fn bypass(&self, spec: &LaunchSpec) -> bool {
        spec.permission_mode == PermissionMode::Bypass && self.bypass_mode.is_some()
    }
}

/// The variables a child inherits from its launcher. Everything else, credentials included,
/// reaches it only through [`LaunchConfig::env`]. `PATH` is passed on without its empty and
/// relative entries, which would resolve against the agent's working directory, and without
/// its entries inside the root.
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

/// Git configuration given to every child through the environment, where it outranks the
/// repository's own `.git/config`: a `core.fsmonitor` there names a program that git runs on
/// `git status`, which a harness may run at startup outside any sandbox it applies to the
/// agent. Claude Code 2.1.326, Codex 0.169 and OpenCode 2.3.0 were each seen to switch it off
/// on their own git calls; this covers the ones they do not.
const GIT_OVERRIDES: &[(&str, &str)] = &[
    ("GIT_CONFIG_COUNT", "1"),
    ("GIT_CONFIG_KEY_0", "core.fsmonitor"),
    ("GIT_CONFIG_VALUE_0", "false"),
];

/// Where basemind keeps its store and its comms rendezvous. A launched agent has to see the
/// launcher's values, or its `basemind serve` talks to a different daemon.
const SHARED_STATE_ENV: &[&str] = &["BASEMIND_DATA_HOME", "BASEMIND_COMMS_DIR"];

/// `path` as UTF-8, which every path written into a harness's protocol has to be.
pub(crate) fn utf8(path: &Path) -> Result<&str, LaunchError> {
    path.to_str().ok_or_else(|| LaunchError::NonUtf8Path(path.to_owned()))
}

/// Whether `name` can be written into every harness's configuration: `[A-Za-z0-9_-]{1,64}`.
fn mcp_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// `value` as a search path without its empty and relative entries, and without the entries
/// inside `root` (as given or resolved), where the agent could plant what the harness's own
/// lookups (`#!/usr/bin/env node`, helper binaries) then find.
fn absolute_search_path(value: &std::ffi::OsStr, root: &Path) -> OsString {
    let canonical_root = root.canonicalize().ok();
    let inside = |dir: &Path| {
        let canonical = dir.canonicalize().ok();
        [Some(dir), canonical.as_deref()]
            .into_iter()
            .flatten()
            .any(|dir| dir.starts_with(root) || canonical_root.as_deref().is_some_and(|root| dir.starts_with(root)))
    };
    let kept = std::env::split_paths(value).filter(|dir| dir.is_absolute() && !inside(dir));
    std::env::join_paths(kept).unwrap_or_default()
}

/// The absolute path `executable` names: itself when absolute, else the first match on the
/// absolute entries of `search`. A relative path with a directory part is refused, and so is
/// an executable that lies inside `root` as given, after resolving links, or through any link
/// on the way, where the launched agent, or one before it, may have written it. The returned
/// path keeps its links: some harnesses find their own files from the path they were started
/// by. The check is not atomic with the exec, and outside the root is not the same as not
/// writable by the agent.
pub(crate) fn resolve_executable(
    executable: &Path,
    search: Option<&std::ffi::OsStr>,
    root: &Path,
) -> Result<PathBuf, LaunchError> {
    let invalid = |reason| LaunchError::InvalidExecutable {
        path: executable.to_owned(),
        reason,
    };
    let found = if executable.is_absolute() {
        executable.to_owned()
    } else if executable.components().count() == 1 && executable.file_name().is_some() {
        let names = command_names(executable);
        let dirs = search.map(std::env::split_paths).into_iter().flatten();
        dirs.filter(|dir| dir.is_absolute())
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|candidate| is_executable(candidate))
            .ok_or_else(|| invalid("it is not on the launcher's PATH"))?
    } else {
        return Err(invalid("a relative path would resolve against the working directory"));
    };
    let canonical = found.canonicalize().map_err(|source| LaunchError::Path {
        path: found.clone(),
        source,
    })?;
    if canonical.starts_with(root) || link_through(&found, root) {
        return Err(invalid("it lies inside the root, where the agent can write"));
    }
    if cfg!(windows) && is_batch(&found) {
        return Err(invalid(
            "a batch file runs through cmd.exe, which looks for the programs it calls in the working \
             directory first; name the program it starts instead",
        ));
    }
    Ok(found)
}

/// Whether `path` is a Windows batch file (`.cmd`, `.bat`), such as the shim an npm install
/// puts on `PATH`.
fn is_batch(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat"))
}

/// The file names a bare command is looked up as in each search directory: the name itself, or
/// on Windows the name with each of the launcher's `PATHEXT` extensions (`claude.cmd`,
/// `codex.exe`) unless it already ends in one. The extensionless file an npm install puts beside
/// its `.cmd` shim is a POSIX script Windows cannot start, so it is not a candidate there.
fn command_names(name: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        with_pathext(name, std::env::var_os("PATHEXT").as_deref())
    }
    #[cfg(not(windows))]
    {
        vec![name.to_owned()]
    }
}

/// `name` alone when its extension is one of `pathext` (`;`-separated, Windows' default when
/// unset), compared without case, else `name` with each of them appended.
#[cfg(any(windows, test))]
fn with_pathext(name: &Path, pathext: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let pathext = pathext
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    let extensions: Vec<&str> = pathext.split(';').filter(|extension| !extension.is_empty()).collect();
    let own = name.extension().and_then(std::ffi::OsStr::to_str);
    let runnable = |own: &str| {
        extensions.iter().any(|extension| {
            extension
                .strip_prefix('.')
                .is_some_and(|extension| extension.eq_ignore_ascii_case(own))
        })
    };
    if own.is_some_and(runnable) {
        return vec![name.to_owned()];
    }
    extensions
        .into_iter()
        .map(|extension| {
            let mut file = name.as_os_str().to_owned();
            file.push(extension);
            PathBuf::from(file)
        })
        .collect()
}

/// The most links followed from an executable before it counts as reaching the root.
const MAX_LINK_HOPS: usize = 40;

/// Whether `path` or a link on the way from it to its target lies inside `root`, or in a
/// directory that resolves into it: the agent could point such a link elsewhere before the
/// launch execs the path. Each hop is checked as written and with its directory resolved.
fn link_through(path: &Path, root: &Path) -> bool {
    let mut hop = path.to_owned();
    for _ in 0..MAX_LINK_HOPS {
        let parent = hop.parent().map(|dir| dir.canonicalize());
        if hop.starts_with(root) || parent.is_some_and(|dir| dir.map_or(true, |dir| dir.starts_with(root))) {
            return true;
        }
        let Ok(target) = std::fs::read_link(&hop) else {
            return false;
        };
        hop = match hop.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
    }
    true
}

/// Whether `path` is a file this process could execute.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    metadata.is_file()
}

/// The canonical root and the canonical working directory inside it.
fn confine(root: &Path, working_dir: Option<&Path>) -> Result<(PathBuf, PathBuf), LaunchError> {
    let resolve = |path: &Path| {
        path.canonicalize().map_err(|source| LaunchError::Path {
            path: path.to_owned(),
            source,
        })
    };
    let root = resolve(root)?;
    let dir = match working_dir {
        Some(dir) => {
            let joined = root.join(dir);
            match joined.canonicalize() {
                Ok(dir) => dir,
                Err(_) => return Err(LaunchError::WorkingDirOutsideRoot { dir: joined, root }),
            }
        }
        None => root.clone(),
    };
    if !dir.starts_with(&root) || !dir.is_dir() {
        return Err(LaunchError::WorkingDirOutsideRoot { dir, root });
    }
    Ok((root, dir))
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
