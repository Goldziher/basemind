//! The one identity resolver of agent comms v2 (ADR-0021).
//!
//! `serve`, the CLI and the hooks all resolve who they are through [`resolve`], so a hook reads the
//! inbox its MCP session posts to. The id is a function of the environment, the config and the
//! checkout root only: no pid, no clock and no file, which is what makes it survive a reconnect.
//!
//! [`resolve`] is pure and takes everything it needs in [`IdentityInputs`]; [`resolve_from_env`] is
//! the thin wrapper that reads the process environment and canonicalises the root.

use std::path::{Component, Path, PathBuf};

use super::api::rejected_id_notice;
use super::error::CommsError;
use super::ids::{Actor, MAX_SUB_NAME_LEN, Principal, SubName, bounded_echo};
pub use crate::comms::identity::AGENT_ID_ENV;

/// Env var naming the host program (`claude-code`, `codex`, …) for the derived id. The plugin
/// launcher and the plugin hooks set it from one shared script.
pub const HOST_ENV: &str = "BASEMIND_HOST";

/// The `<host>` of a derived id when [`HOST_ENV`] is unset or holds no usable character.
pub const DEFAULT_HOST: &str = "agent";

/// Longest `<host>` part of a derived id, in bytes.
pub const MAX_HOST_LEN: usize = 16;

/// Longest `<repo>` part of a derived id, in bytes.
pub const MAX_REPO_LEN: usize = 24;

/// Hex digits of the root's blake3 hash that end a derived id.
pub const ROOT_HASH_HEX_LEN: usize = 6;

/// How much of a Claude Code `agent_id` a hook-injected sub-name carries, in bytes.
pub const HOOK_AGENT_ID_LEN: usize = 8;

/// Separator between the parts of a derived id and of a hook-injected sub-name.
const PART_SEPARATOR: char = '-';

/// Where an identity came from, in the order the tiers are tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdentityTier {
    /// `BASEMIND_AGENT_ID`, verbatim.
    Env,
    /// The `comms.agent_id` config key.
    Config,
    /// Derived from the host and the checkout root.
    Derived,
}

impl IdentityTier {
    /// The setting a user edits to change this tier's value.
    pub fn label(self) -> &'static str {
        match self {
            Self::Env => AGENT_ID_ENV,
            Self::Config => "comms.agent_id",
            Self::Derived => "derived",
        }
    }
}

/// Everything identity resolution depends on, gathered by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityInputs<'a> {
    /// The raw `BASEMIND_AGENT_ID` value, when set.
    pub env_agent_id: Option<String>,
    /// The raw `comms.agent_id` config value, when set.
    pub config_agent_id: Option<String>,
    /// The raw `BASEMIND_HOST` value, when set.
    pub env_host: Option<String>,
    /// The checkout root, canonical (see [`canonical_root`]): its bytes are hashed as given.
    pub root: &'a Path,
}

/// A configured id that failed validation and was skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedId {
    /// The tier the value was configured in.
    pub tier: IdentityTier,
    /// The refused value, cut to the bounded echo length.
    pub value: String,
}

/// The outcome of identity resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    /// The id the session acts as.
    pub principal: Principal,
    /// The tier that produced it.
    pub tier: IdentityTier,
    /// Configured ids that were refused on the way, in tier order.
    pub rejected: Vec<RejectedId>,
}

impl ResolvedIdentity {
    /// The rejected-id notices for the session's first agent-tool reply: one per refused value.
    pub fn notices(&self) -> Vec<String> {
        self.rejected
            .iter()
            .map(|rejected| rejected_id_notice(&rejected.value, self.principal.as_str()))
            .collect()
    }
}

/// Resolve the identity for `inputs`: the environment id, else the config id, else the derived id.
///
/// An empty configured value is the value left unset. Any other value that is not a valid
/// [`Principal`] (one containing `.` included) is skipped and reported in
/// [`ResolvedIdentity::rejected`]. Pure: the same inputs always give the same result.
pub fn resolve(inputs: &IdentityInputs<'_>) -> ResolvedIdentity {
    let mut rejected = Vec::new();
    let configured = [
        (IdentityTier::Env, inputs.env_agent_id.as_deref()),
        (IdentityTier::Config, inputs.config_agent_id.as_deref()),
    ];
    for (tier, candidate) in configured {
        let Some(candidate) = candidate.filter(|candidate| !candidate.is_empty()) else {
            continue;
        };
        match Principal::parse(candidate) {
            Ok(principal) => {
                return ResolvedIdentity {
                    principal,
                    tier,
                    rejected,
                };
            }
            Err(_) => rejected.push(RejectedId {
                tier,
                value: bounded_echo(candidate),
            }),
        }
    }
    ResolvedIdentity {
        principal: derived_principal(inputs.env_host.as_deref(), inputs.root),
        tier: IdentityTier::Derived,
        rejected,
    }
}

/// The derived id: `<host>-<repo>-<6 hex of blake3(root)>`.
///
/// `root` is hashed byte for byte, so it must already be canonical for two spellings of one
/// checkout to agree.
pub fn derived_principal(host: Option<&str>, root: &Path) -> Principal {
    let hash = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
    let id = format!(
        "{}{PART_SEPARATOR}{}{PART_SEPARATOR}{}",
        host_part(host),
        repo_part(root),
        &hash[..ROOT_HASH_HEX_LEN]
    );
    Principal::parse(id).expect("a derived id is at most 48 bytes of a-z, 0-9 and `-`")
}

/// The `<host>` of a derived id: ASCII-lowercased, every byte outside `[a-z0-9]` dropped, cut to
/// [`MAX_HOST_LEN`]; [`DEFAULT_HOST`] when nothing is left.
pub fn host_part(host: Option<&str>) -> String {
    let reduced: String = host
        .unwrap_or_default()
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .filter(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        .take(MAX_HOST_LEN)
        .map(char::from)
        .collect();
    if reduced.is_empty() {
        DEFAULT_HOST.to_string()
    } else {
        reduced
    }
}

/// The `<repo>` of a derived id: the root's final path component, ASCII-lowercased, every byte
/// outside `[a-z0-9-]` replaced by `-`, cut to [`MAX_REPO_LEN`].
///
/// The replacement is per byte, so a multi-byte character or an undecodable byte sequence becomes
/// one `-` per byte and a non-UTF-8 name needs no lossy decoding. Runs of `-` and a leading or
/// trailing `-` are kept as they are. A root without a final component (`/`, the empty path, one
/// ending in `..`) gives the empty string: the derived id then reads `<host>--<hash>` and is still
/// told apart by its hash.
pub fn repo_part(root: &Path) -> String {
    let Some(name) = root.file_name() else {
        return String::new();
    };
    name.as_encoded_bytes()
        .iter()
        .take(MAX_REPO_LEN)
        .map(|byte| byte.to_ascii_lowercase())
        .map(|byte| {
            if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' {
                char::from(byte)
            } else {
                PART_SEPARATOR
            }
        })
        .collect()
}

/// Resolve the identity of this process for the checkout at `root`.
///
/// Reads [`AGENT_ID_ENV`] and [`HOST_ENV`], canonicalises `root` with [`canonical_root`], and logs
/// a warning for every configured id that was refused.
pub fn resolve_from_env(config_agent_id: Option<String>, root: &Path) -> ResolvedIdentity {
    let resolved = resolve_with(process_env, config_agent_id, root);
    for rejected in &resolved.rejected {
        tracing::warn!(
            setting = rejected.tier.label(),
            value = %rejected.value,
            in_use = resolved.principal.as_str(),
            "comms: configured agent id is not a valid id and is not used"
        );
    }
    resolved
}

/// [`resolve_from_env`] over an explicit environment lookup.
fn resolve_with(
    env: impl Fn(&str) -> Option<String>,
    config_agent_id: Option<String>,
    root: &Path,
) -> ResolvedIdentity {
    let root = canonical_root(root);
    resolve(&IdentityInputs {
        env_agent_id: env(AGENT_ID_ENV),
        config_agent_id,
        env_host: env(HOST_ENV),
        root: &root,
    })
}

/// A process environment variable, decoded lossily so a non-Unicode id is refused and reported
/// instead of being read as unset.
fn process_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// The form of `root` the derived id hashes: the canonical path, symlinks resolved.
///
/// When `root` cannot be canonicalised (it does not exist, or a parent is unreadable) the path is
/// made absolute against the working directory and [`lexically_clean`]ed instead, so `repo/.` and
/// `repo/` still agree.
pub fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| {
        let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        lexically_clean(&absolute)
    })
}

/// `path` with `.` components and redundant separators removed and `..` folded into the component
/// before it, without touching the filesystem. A `..` at the root is dropped; one that has nothing
/// to fold into, in a relative path, is kept.
pub fn lexically_clean(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match clean.components().next_back() {
                Some(Component::Normal(_)) => {
                    clean.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir | Component::CurDir) | None => clean.push(component),
            },
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => clean.push(component),
        }
    }
    clean
}

/// The actor a call acts as: `principal` itself, or its sub-identity `as_`.
///
/// An empty `as_` is the field left out. A name outside the sub-name grammar, or a composed id
/// over the id length limit, is refused.
pub fn compose(principal: &Principal, as_: Option<&str>) -> Result<Actor, CommsError> {
    let sub = as_.filter(|name| !name.is_empty()).map(SubName::parse).transpose()?;
    Ok(Actor::new(principal, sub.as_ref())?)
}

/// The sub-name the Claude Code `PreToolUse` hook injects for a subagent that passed no `as`:
/// `<agent_type>-<first 8 of agent_id>`, reduced to the sub-name grammar.
///
/// Each part is ASCII-lowercased and every byte outside `[a-z0-9_-]` becomes `-`. The id part is
/// then cut to [`HOOK_AGENT_ID_LEN`] bytes and the type part to what is left of the 32-byte
/// sub-name, so a long agent type never pushes out the id that tells two subagents of that type
/// apart. An empty part is left out together with its separator. `None` when no letter, digit or
/// `_` survives, because a name of only `-` identifies nothing.
pub fn hook_sub_name(agent_type: &str, agent_id: &str) -> Option<SubName> {
    let id = reduce_to_sub_name(agent_id, HOOK_AGENT_ID_LEN);
    let type_budget = match id.len() {
        0 => MAX_SUB_NAME_LEN,
        id_len => MAX_SUB_NAME_LEN - id_len - PART_SEPARATOR.len_utf8(),
    };
    let mut name = reduce_to_sub_name(agent_type, type_budget);
    if !name.is_empty() && !id.is_empty() {
        name.push(PART_SEPARATOR);
    }
    name.push_str(&id);
    if name.bytes().all(|byte| byte == b'-') {
        return None;
    }
    SubName::parse(name).ok()
}

/// `value` ASCII-lowercased, with every byte outside `[a-z0-9_-]` replaced by `-`, cut to `max`.
fn reduce_to_sub_name(value: &str, max: usize) -> String {
    value
        .bytes()
        .take(max)
        .map(|byte| byte.to_ascii_lowercase())
        .map(|byte| {
            if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-') {
                char::from(byte)
            } else {
                PART_SEPARATOR
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
