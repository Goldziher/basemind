//! The operator's agent-launch policy: read only from the user-level `agents.toml`
//! ([`crate::config::agents_policy::policy_path`]) and turned into the `basemind-agents` policy
//! and one launch config per allowed harness.
//!
//! The file is the only source (review 9 must-fix 2). It is opened without following a link and
//! refused unless it is a regular file with one link, owned by this user and writable by nobody
//! else, in a directory owned by this user (or root) that nobody else can write, outside the lead's
//! workspace. Its schema can express only what is safe to let a lead model use: no bypass or
//! unverified-mode opt-in, no approval policy, no `launcher_may_allow`, no launcher list and no
//! depth (`max_depth` is 1, `max_allows` 0), and no pre-approved tool: every launch pre-approves the
//! injected server's agent comms tools and nothing else (ADR-0044). Executables are absolute paths; credentials are named
//! (`pass_env`) and read from the lead's own environment, never written in the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use basemind_agents::{
    AgentIdentity, Budget, FlavorPolicy, Harness, HarnessKind, LaunchConfig, LaunchPolicy, McpServerSpec,
    PermissionMode, PolicySpec,
};
use serde::Deserialize;

/// The largest policy file read.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Defaults for limits the file leaves out: small, as ADR-0040 asks.
const DEFAULT_MAX_CONCURRENT: u32 = 2;
const DEFAULT_WALL_TIME_SECS: u64 = 30 * 60;
const DEFAULT_MAX_LAUNCHES: u32 = 16;
const DEFAULT_MAX_TURNS: u32 = 8;
const DEFAULT_STOP_GRACE_SECS: u64 = 5;
/// The identity a launch config carries; the supervisor replaces it with the launched principal.
const TEMPLATE_IDENTITY: &str = "launched-template";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    agents: Option<AgentsTable>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsTable {
    #[serde(default)]
    launch: Option<LaunchTable>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchTable {
    #[serde(default)]
    enabled: bool,
    worktrees_root: Option<PathBuf>,
    max_concurrent: Option<u32>,
    max_children: Option<u32>,
    wall_time_secs: Option<u64>,
    max_launches: Option<u32>,
    max_turns: Option<u32>,
    audit_max_bytes: Option<u64>,
    #[serde(default)]
    harness: HarnessTables,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessTables {
    claude: Option<FlavorTable>,
    codex: Option<FlavorTable>,
    opencode: Option<FlavorTable>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlavorTable {
    executable: PathBuf,
    #[serde(default)]
    permission_mode: ModeName,
    #[serde(default)]
    pass_env: Vec<String>,
    max_turns: Option<u32>,
    wall_time_secs: Option<u64>,
    stop_grace_secs: Option<u64>,
}

/// The permission modes the file may name. There is no bypass: a lead model must not be able to
/// start an agent that never asks.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModeName {
    #[default]
    Supervised,
    ReadOnly,
    AcceptEdits,
}

impl From<ModeName> for PermissionMode {
    fn from(mode: ModeName) -> Self {
        match mode {
            ModeName::Supervised => PermissionMode::Supervised,
            ModeName::ReadOnly => PermissionMode::ReadOnly,
            ModeName::AcceptEdits => PermissionMode::AcceptEdits,
        }
    }
}

/// What the lead may launch, built from the operator's file.
#[derive(Debug)]
pub struct LeadPolicy {
    /// The validated policy: `max_depth` 1, no allows, the lead as its only launcher.
    pub policy: LaunchPolicy,
    /// One launch config per allowed harness, rooted at [`Self::worktrees_root`].
    pub configs: Vec<LaunchConfig>,
    /// The canonical directory every launched agent's worktree lies in.
    pub worktrees_root: PathBuf,
    /// The audit log's cap, when the file sets one.
    pub audit_max_bytes: Option<u64>,
}

/// What the operator's file says.
#[derive(Debug)]
pub enum Loaded {
    /// There is no file: launching is off.
    Absent,
    /// The file turns launching off, or names no harness.
    Disabled,
    /// Launching is on.
    Enabled(LeadPolicy),
}

/// Why the operator's file was refused. Launching then stays off.
#[derive(Debug, thiserror::Error)]
pub enum PolicyFileError {
    /// The file could not be opened or read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The file, or its directory, is not one only this user controls.
    #[error("refusing {path}: {reason}")]
    Unsafe {
        /// The file or directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The file is not valid TOML of the policy's shape.
    #[error("invalid {path}: {source}")]
    Toml {
        /// The file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: toml::de::Error,
    },
    /// A value breaks a rule of the policy.
    #[error("invalid [agents.launch] in {path}: {reason}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// The rule.
        reason: String,
    },
}

/// Inputs the policy is built against, apart from the file.
pub struct Context<'a> {
    /// The lead's canonical workspace root.
    pub lead_root: &'a Path,
    /// The principal the lead registers as its only launcher.
    pub launcher: &'a str,
    /// The `basemind` executable injected into launched agents as their MCP server.
    pub mcp_command: &'a Path,
    /// Reads a credential named in `pass_env` from the lead's environment.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// Read and build the operator's policy from `path`.
///
/// # Errors
///
/// [`PolicyFileError`] for a file that cannot be read, is not one only this user controls, does
/// not parse, or breaks a rule of the policy.
pub fn load(path: &Path, context: &Context<'_>) -> Result<Loaded, PolicyFileError> {
    let Some(text) = read_operator_file(path, context.lead_root)? else {
        return Ok(Loaded::Absent);
    };
    let file: PolicyFile = toml::from_str(&text).map_err(|source| PolicyFileError::Toml {
        path: path.to_owned(),
        source,
    })?;
    let Some(launch) = file.agents.and_then(|agents| agents.launch) else {
        return Ok(Loaded::Disabled);
    };
    if !launch.enabled {
        return Ok(Loaded::Disabled);
    }
    let invalid = |reason: String| PolicyFileError::Invalid {
        path: path.to_owned(),
        reason,
    };
    build(launch, context).map(Loaded::Enabled).map_err(invalid)
}

fn build(launch: LaunchTable, context: &Context<'_>) -> Result<LeadPolicy, String> {
    let worktrees_root = worktrees_root(launch.worktrees_root.as_deref(), context.lead_root)?;
    let max_concurrent = launch.max_concurrent.unwrap_or(DEFAULT_MAX_CONCURRENT);
    let wall_time = Duration::from_secs(launch.wall_time_secs.unwrap_or(DEFAULT_WALL_TIME_SECS));
    let max_turns = launch.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    let flavors = [
        (HarnessKind::ClaudeCode, launch.harness.claude),
        (HarnessKind::Codex, launch.harness.codex),
        (
            HarnessKind::Acp(basemind_agents::AcpFlavor::OpenCode),
            launch.harness.opencode,
        ),
    ];
    let mut rules = Vec::new();
    let mut configs = Vec::new();
    for (kind, table) in flavors {
        let Some(table) = table else { continue };
        let config = flavor_config(kind, table, &worktrees_root, wall_time, max_turns, context)?;
        let mut rule = FlavorPolicy::supervised(kind);
        rule.permission_mode = config.permission_mode;
        rule.allow_tools = comms_tools();
        rules.push(rule);
        configs.push(config);
    }
    if rules.is_empty() {
        return Err("launching is enabled but no [agents.launch.harness.<name>] table names a harness".to_owned());
    }
    let mut spec = PolicySpec::default();
    spec.flavors = rules;
    spec.launchers = vec![context.launcher.to_owned()];
    spec.max_depth = 1;
    spec.max_concurrent = max_concurrent;
    spec.max_children = launch.max_children.unwrap_or(max_concurrent);
    spec.wall_time = wall_time;
    spec.budget = Budget {
        max_launches: launch.max_launches.unwrap_or(DEFAULT_MAX_LAUNCHES),
        max_turns,
        max_allows: 0,
    };
    let policy = LaunchPolicy::new(spec).map_err(|error| error.to_string())?;
    Ok(LeadPolicy {
        policy,
        configs,
        worktrees_root,
        audit_max_bytes: launch.audit_max_bytes,
    })
}

/// The injected server's tools every launch pre-approves: the agent comms tools, so the launched
/// agent can read and answer the thread its launcher opened (ADR-0044). Every other tool, of the
/// server or the harness, still asks and is denied.
fn comms_tools() -> Vec<String> {
    crate::mcp::agent_tools::AGENT_TOOLS
        .iter()
        .map(|tool| (*tool).to_owned())
        .collect()
}

/// The canonical worktrees root: absolute, existing, and neither the lead's root, nor above it, nor
/// inside it. A launched agent is not sandboxed to its worktree, but what it writes there is not
/// in a tree the lead's harness builds, hooks or loads settings from.
fn worktrees_root(configured: Option<&Path>, lead_root: &Path) -> Result<PathBuf, String> {
    let configured = configured.ok_or("worktrees_root is required: the directory launched agents' worktrees are in")?;
    if !configured.is_absolute() {
        return Err("worktrees_root must be an absolute path".to_owned());
    }
    let root = configured
        .canonicalize()
        .map_err(|error| format!("worktrees_root {}: {error}", configured.display()))?;
    if !root.is_dir() {
        return Err(format!("worktrees_root {} is not a directory", root.display()));
    }
    if overlaps(&root, lead_root) {
        return Err(format!(
            "worktrees_root {} is, contains or lies inside the lead's workspace {}: launched agents need \
             worktrees of their own",
            root.display(),
            lead_root.display()
        ));
    }
    Ok(root)
}

/// Whether one of two canonical paths is, or lies inside, the other.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn flavor_config(
    kind: HarnessKind,
    table: FlavorTable,
    worktrees_root: &Path,
    wall_time: Duration,
    max_turns: u32,
    context: &Context<'_>,
) -> Result<LaunchConfig, String> {
    let name = kind.id();
    // Modes the adapter refuses at every launch: Codex has no accept-edits mode, and OpenCode's
    // other modes need an unverified-mode opt-in the file cannot express.
    let launchable = match (kind, table.permission_mode) {
        (HarnessKind::Codex, ModeName::AcceptEdits) => false,
        (HarnessKind::Acp(_), ModeName::ReadOnly | ModeName::AcceptEdits) => false,
        _ => true,
    };
    if !launchable {
        return Err(format!(
            "harness.{name}.permission_mode {:?} is a mode {name} is never launched in",
            table.permission_mode
        ));
    }
    if !table.executable.is_absolute() {
        return Err(format!(
            "harness.{name}.executable must be an absolute path: a name looked up on PATH could be one a \
             launched agent planted"
        ));
    }
    if !context.mcp_command.is_absolute() {
        return Err("the basemind executable injected into launched agents has no absolute path".to_owned());
    }
    let identity = AgentIdentity::new(TEMPLATE_IDENTITY).map_err(|error| error.to_string())?;
    let mut config = LaunchConfig::new(
        Harness::new(kind).with_executable(table.executable),
        worktrees_root,
        identity,
    );
    let mut env = BTreeMap::new();
    for variable in table.pass_env {
        let value = (context.env)(&variable).ok_or_else(|| {
            format!("harness.{name}.pass_env names {variable}, which is not set in the lead's environment")
        })?;
        env.insert(variable, value);
    }
    config.env = env;
    let mut server = McpServerSpec::basemind_serve(context.mcp_command);
    server.allow_tools = comms_tools();
    config.mcp_server = Some(server);
    config.permission_mode = table.permission_mode.into();
    config.limits.wall_time = table
        .wall_time_secs
        .map_or(wall_time, Duration::from_secs)
        .min(wall_time);
    config.limits.max_turns = table.max_turns.unwrap_or(max_turns).min(max_turns);
    config.limits.stop_grace = Duration::from_secs(table.stop_grace_secs.unwrap_or(DEFAULT_STOP_GRACE_SECS));
    Ok(config)
}

/// The file's text, `None` when it does not exist. Refused unless only this user controls it.
fn read_operator_file(path: &Path, lead_root: &Path) -> Result<Option<String>, PolicyFileError> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let unsafe_file = |path: &Path, reason| PolicyFileError::Unsafe {
        path: path.to_owned(),
        reason,
    };
    let io = |source| PolicyFileError::Io {
        path: path.to_owned(),
        source,
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(unsafe_file(path, "it is a symbolic link"));
        }
        Err(error) => return Err(io(error)),
    };
    let meta = file.metadata().map_err(io)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if !meta.file_type().is_file() {
        return Err(unsafe_file(path, "it is not a regular file"));
    }
    if meta.nlink() != 1 {
        return Err(unsafe_file(path, "it has another hard link"));
    }
    if meta.uid() != uid {
        return Err(unsafe_file(path, "another user owns it"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(unsafe_file(path, "its group or other users can write it"));
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    let dir = std::fs::metadata(parent).map_err(io)?;
    if dir.uid() != uid && dir.uid() != 0 {
        return Err(unsafe_file(parent, "another user owns the directory it is in"));
    }
    if dir.mode() & 0o022 != 0 {
        return Err(unsafe_file(parent, "its directory's group or other users can write it"));
    }
    let canonical = parent.canonicalize().map_err(io)?;
    if canonical.starts_with(lead_root) {
        return Err(unsafe_file(
            path,
            "it lies inside the lead's workspace, where an agent can write",
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(unsafe_file(path, "it is larger than 256 KiB"));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text).map_err(io)?;
    Ok(Some(text))
}

#[cfg(test)]
#[path = "policy_file_tests.rs"]
mod tests;
