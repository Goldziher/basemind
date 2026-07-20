//! The ONE agent-identity resolver, shared by `basemind serve` (MCP) and every CLI verb.
//!
//! Identity used to be resolved in four places — [`crate::mcp::identity`] plus three hand-copied
//! CLI variants — and they drifted. The CLI copies read the LEGACY in-repo cache path
//! (`<root>/.basemind/agent-id`), which stopped existing when the cache went machine-global, so
//! they always fell through to a single hardcoded constant: every agent on the machine,
//! in every repo, shared one identity and one inbox. Messages addressed to one agent were
//! delivered as the other's OWN messages, so its inbox showed zero unread. This module exists so
//! that logic lives EXACTLY ONCE and cannot drift again.
//!
//! Resolution is tiered; an invalid candidate falls THROUGH to the next tier rather than failing
//! the process (identity must never be able to break server boot):
//!
//! 1. [`IdentitySource::Env`] — `BASEMIND_AGENT_ID`, the explicit per-process override.
//! 2. [`IdentitySource::Config`] — `config.comms.agent_id`, the workspace's declared identity.
//! 3. [`IdentitySource::Workspace`] — a generated id persisted at
//!    `<workspace_cache_dir>/agent-id`. This is the tier that fixes the collision: the path is the
//!    machine-global PER-WORKSPACE cache dir ([`crate::store::workspace_cache_dir`], keyed by a
//!    hash of the canonical root), so a CLI call in a repo adopts the identity the `serve` session
//!    in THAT repo persisted, while a different repo necessarily gets a different id.
//! 4. [`IdentitySource::Generated`] — a process-unique id, used only when the workspace tier
//!    cannot persist (unwritable cache). NEVER a shared constant: an unpersistable identity is
//!    ephemeral, never one another process can accidentally adopt.
//!
//! Inputs arrive as PARAMETERS ([`IdentityRequest`]), not ambient process env, so resolution is
//! testable without mutating a process-global that other tests race on.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::comms::ids::AgentId;

/// The tier-1 environment override.
pub const AGENT_ID_ENV: &str = "BASEMIND_AGENT_ID";

/// File holding the generated-and-persisted per-workspace agent id, written under the workspace's
/// machine-global cache dir the first time resolution reaches [`IdentitySource::Workspace`].
pub const AGENT_ID_FILE: &str = "agent-id";

/// Directory holding the claim ledger, one file per agent id, under the machine-global cache root.
/// See [`IdentityCollision`] for what it buys.
pub const CLAIMS_DIR: &str = "agents";

/// Which tier produced an [`AgentIdentity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentitySource {
    /// Tier 1: the `BASEMIND_AGENT_ID` environment override.
    Env,
    /// Tier 2: `config.comms.agent_id`.
    Config,
    /// Tier 3: generated once per workspace and persisted in that workspace's cache dir.
    Workspace,
    /// Tier 4: generated per process because the workspace tier could not persist. Ephemeral.
    Generated,
}

/// Two different workspaces laying claim to ONE agent id — the foot-gun that produced the original
/// bug, and the only way it can still happen now that generated ids are per-workspace: a user
/// pinning the same explicit id (env or config) in two repos.
///
/// This is reported, never enforced. Blocking the second claimant would break the legitimate case
/// it is indistinguishable from at this layer (an agent reconnecting after its previous process
/// died), and locking a user out of their own inbox is a worse failure than the one being
/// prevented. Making the collision LOUD is the whole ask: silence is what made the original bug
/// take days to find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityCollision {
    /// The contested agent id.
    pub agent_id: String,
    /// The workspace root that claimed the id before us.
    pub claimed_by_root: PathBuf,
    /// The pid that claimed the id before us. Diagnostics only — it is NOT probed for liveness
    /// (a cross-workspace reuse is worth surfacing whether or not the other process is still up).
    pub claimed_by_pid: u32,
    /// The workspace root claiming the id now.
    pub our_root: PathBuf,
}

/// Where identity state lives on disk. Split out of resolution so tests can point it at a temp
/// dir instead of `BASEMIND_DATA_HOME` (a process-global that parallel tests would race on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityPaths {
    /// The workspace's machine-global cache dir; holds [`AGENT_ID_FILE`].
    pub workspace_cache_dir: PathBuf,
    /// The machine-global claim ledger dir; holds one claim file per agent id.
    pub claims_dir: PathBuf,
}

impl IdentityPaths {
    /// The real machine-global paths for `root`.
    ///
    /// Deliberately a pure path computation: it does NOT open a [`crate::store::Store`], because
    /// the comms CLI verbs are an offline/daemon path that must not take the repo index lock just
    /// to learn who they are.
    pub fn for_root(root: &Path) -> Self {
        Self {
            workspace_cache_dir: crate::store::workspace_cache_dir(root),
            claims_dir: claims_dir(),
        }
    }
}

/// The machine-global claim ledger dir: `cache_root()/cache/agents/`.
pub fn claims_dir() -> PathBuf {
    crate::store::cache_root()
        .join(crate::store::CACHE_DIR)
        .join(CLAIMS_DIR)
}

/// Everything [`resolve`] is allowed to look at, passed explicitly.
#[derive(Clone, Debug)]
pub struct IdentityRequest<'a> {
    /// The workspace root this identity belongs to.
    pub root: &'a Path,
    /// Where identity state is read from and written to.
    pub paths: IdentityPaths,
    /// Tier 1 candidate — the raw `BASEMIND_AGENT_ID` value, if set.
    pub env_agent_id: Option<String>,
    /// Tier 2 candidate — the raw `config.comms.agent_id` value, if set.
    pub config_agent_id: Option<String>,
}

impl<'a> IdentityRequest<'a> {
    /// The production request: real cache paths, the real env var, and the caller's configured id
    /// (`config.comms.agent_id`). Takes the id rather than a `&Config` so identity resolution has
    /// no dependency on the config types — it only ever needed the one field.
    pub fn from_process(root: &'a Path, config_agent_id: Option<String>) -> Self {
        Self {
            root,
            paths: IdentityPaths::for_root(root),
            env_agent_id: std::env::var(AGENT_ID_ENV).ok(),
            config_agent_id,
        }
    }
}

/// A resolved identity: the id, the tier it came from, and any cross-workspace claim collision.
#[derive(Clone, Debug)]
pub struct AgentIdentity {
    id: AgentId,
    source: IdentitySource,
    collision: Option<IdentityCollision>,
}

impl AgentIdentity {
    /// The resolved id.
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// Consume the identity, yielding the id.
    pub fn into_id(self) -> AgentId {
        self.id
    }

    /// The tier that produced the id.
    pub fn source(&self) -> IdentitySource {
        self.source
    }

    /// The detected cross-workspace collision, if any.
    pub fn collision(&self) -> Option<&IdentityCollision> {
        self.collision.as_ref()
    }

    /// The collision rendered for a human, naming BOTH claimants. `None` when there is no
    /// collision. Front-ends surface this: `serve` logs it (see [`resolve`], which also emits a
    /// `tracing::warn!`), the CLI prints it to stderr.
    pub fn collision_warning(&self) -> Option<String> {
        let c = self.collision.as_ref()?;
        Some(format!(
            "agent id {:?} is already claimed by another workspace (pid {} in {}); this process is \
             in {}. Both will share ONE inbox — each will see the other's messages as its own. \
             Unset BASEMIND_AGENT_ID (or comms.agent_id) in one of them to get distinct identities.",
            c.agent_id,
            c.claimed_by_pid,
            c.claimed_by_root.display(),
            c.our_root.display(),
        ))
    }
}

/// Resolve an agent identity from explicit inputs, then record the claim and report any
/// cross-workspace collision (also emitted as a `tracing::warn!`, so `serve` surfaces it without
/// any front-end work).
pub fn resolve(request: &IdentityRequest<'_>) -> AgentIdentity {
    let (id, source) = resolve_id(request);
    let collision = record_claim(&request.paths.claims_dir, &id, request.root);
    let identity = AgentIdentity { id, source, collision };
    if let Some(warning) = identity.collision_warning() {
        tracing::warn!(
            agent_id = %identity.id,
            source = ?identity.source,
            "basemind: AGENT IDENTITY COLLISION — {warning}"
        );
    }
    identity
}

/// The production entry point: resolve for `root` using the real env and cache paths.
pub fn resolve_for_root(root: &Path, config_agent_id: Option<String>) -> AgentIdentity {
    resolve(&IdentityRequest::from_process(root, config_agent_id))
}

/// The identity for a CLI verb run in `root`: the SAME id the `serve` session in this workspace
/// uses, loading `config.comms.agent_id` on the way.
///
/// The one entry point for every CLI call site. It exists because the previous arrangement —
/// three hand-copied resolvers in three CLI modules — is precisely how the collision bug got in;
/// a second copy of this logic must never be worth writing.
///
/// A cross-workspace collision is printed to stderr (the CLI has no `tracing` sink) and the id is
/// still returned: visible, never fatal.
pub fn cli_agent_id(root: &Path) -> AgentId {
    let config_agent_id = crate::config::load(root).ok().and_then(|config| config.comms.agent_id);
    let identity = resolve_for_root(root, config_agent_id);
    if let Some(warning) = identity.collision_warning() {
        eprintln!("warning: {warning}");
    }
    identity.into_id()
}

/// The tiering itself, free of claim bookkeeping.
fn resolve_id(request: &IdentityRequest<'_>) -> (AgentId, IdentitySource) {
    let validated = |candidate: Option<&str>| candidate.and_then(|s| AgentId::parse(s).ok());

    if let Some(id) = validated(request.env_agent_id.as_deref()) {
        return (id, IdentitySource::Env);
    }
    if let Some(id) = validated(request.config_agent_id.as_deref()) {
        return (id, IdentitySource::Config);
    }
    if let Some(id) = load_or_create_workspace_id(&request.paths.workspace_cache_dir) {
        return (id, IdentitySource::Workspace);
    }
    (generated_id("agent"), IdentitySource::Generated)
}

/// Read `<workspace_cache_dir>/agent-id`, generating and persisting one when absent.
///
/// The create is `create_new` (O_EXCL), so two processes racing to seed one workspace cannot end
/// up with two ids: the loser observes `AlreadyExists` and reads the winner's id back. Returns
/// `None` only when the id can be neither read nor written, which drops resolution to the
/// ephemeral tier rather than erroring.
fn load_or_create_workspace_id(workspace_cache_dir: &Path) -> Option<AgentId> {
    let path = workspace_cache_dir.join(AGENT_ID_FILE);
    if let Some(id) = read_agent_id(&path) {
        return Some(id);
    }
    std::fs::create_dir_all(workspace_cache_dir).ok()?;

    let candidate = generated_id("session");
    match std::fs::File::options().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(candidate.as_str().as_bytes()).ok()?;
            Some(candidate)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_agent_id(&path),
        Err(_) => None,
    }
}

/// Read and validate a persisted agent id, treating empty/invalid/unreadable alike as absent.
fn read_agent_id(path: &Path) -> Option<AgentId> {
    let raw = std::fs::read_to_string(path).ok()?;
    AgentId::parse(raw.trim()).ok()
}

/// A process-unique id: `<prefix>-<pid>-<nanos>`, hex, within the `AgentId` alphabet.
fn generated_id(prefix: &str) -> AgentId {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let token = format!("{prefix}-{:x}-{:x}", std::process::id(), nanos);
    AgentId::parse(token).expect("generated id is within the AgentId alphabet")
}

/// One agent id's current claimant, persisted in the ledger.
#[derive(Debug, Serialize, Deserialize)]
struct AgentClaim {
    /// The claimed id (the filename is a hash, so the id is recorded in the body).
    agent_id: String,
    /// The canonical workspace root of the claimant.
    root: PathBuf,
    /// The claimant's pid. Diagnostics only.
    pid: u32,
    /// When the claim was written, Unix-epoch seconds.
    updated_unix: i64,
}

/// Record this process's claim on `id`, returning a collision when the id was last claimed from a
/// DIFFERENT workspace root.
///
/// Same-root re-resolution (a `serve` restart, every subsequent CLI call, an orchestrator's
/// `--as-agent` sub-identities) is a reconnect, not a collision — the root, not the pid, is the
/// discriminator, precisely because the intended behavior is for the CLI and `serve` in one
/// workspace to SHARE an identity across processes.
///
/// Last-writer-wins: the claim is overwritten so the warning fires on each hand-off rather than
/// once, and a stale claim from a dead process cannot wedge the ledger. Best-effort throughout —
/// a ledger that cannot be written degrades to today's behavior (no detection), never an error.
fn record_claim(claims_dir: &Path, id: &AgentId, root: &Path) -> Option<IdentityCollision> {
    let our_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = claims_dir.join(format!(
        "{}.json",
        crate::hashing::hex(&crate::hashing::hash_bytes(id.as_str().as_bytes()))
    ));

    let previous = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<AgentClaim>(&bytes).ok());

    let collision = previous.and_then(|prev| {
        (prev.root != our_root).then(|| IdentityCollision {
            agent_id: id.as_str().to_string(),
            claimed_by_root: prev.root,
            claimed_by_pid: prev.pid,
            our_root: our_root.clone(),
        })
    });

    let claim = AgentClaim {
        agent_id: id.as_str().to_string(),
        root: our_root,
        pid: std::process::id(),
        updated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default(),
    };
    if std::fs::create_dir_all(claims_dir).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&claim)
    {
        let _ = std::fs::write(&path, bytes);
    }
    collision
}
