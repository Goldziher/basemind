//! Room / id derivation helpers for the broker, split out of `daemon.rs` to keep it under the
//! 1000-line module cap. These are the keying rules that decide which room a scope maps to and how
//! ids are sanitized / minted — the broker's auto-join and the `get_or_create_chat_room_for_path`
//! tool both route through [`default_room_for`] / [`repo_room_for`] so they agree byte-for-byte.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::comms::ids::{self, AgentId, RoomId};
use crate::comms::model::{Room, RoomScope, now_micros};
use crate::comms::scope::{self, ScopeChain};

/// Build a scope chain from the optional remote + cwd a client supplied. When `cwd` is given
/// we attempt git discovery to enrich the chain's remote if the client did not supply one.
pub(super) fn build_chain(remote: Option<String>, cwd: Option<std::path::PathBuf>) -> ScopeChain {
    match cwd {
        Some(cwd) => {
            let repo = crate::git::Repo::discover(&cwd).ok();
            let mut chain = scope::scope_chain(&cwd, repo.as_ref());
            if chain.remote.is_none() {
                chain.remote = remote;
            }
            chain
        }
        None => ScopeChain {
            remote,
            cwd: std::path::PathBuf::new(),
            ancestors: Vec::new(),
            // Session context is layered on by `on_hello` after the base chain is built.
            session_id: None,
            parent_agent: None,
        },
    }
}

/// The default room every agent in a scope auto-joins on first sight. Keyed by remote when the
/// agent is in a repo with a remote, else by the repo/workspace path, else Global.
pub(super) fn default_room_for(chain: &ScopeChain) -> Room {
    let (room_id, scope, title) = match (&chain.remote, chain.cwd.as_os_str().is_empty()) {
        (Some(remote), _) => (
            RoomId::parse(sanitize_id(remote)).unwrap_or_else(|_| fallback_room()),
            RoomScope::Remote(remote.clone()),
            format!("workspace: {remote}"),
        ),
        (None, false) => {
            let path = chain.cwd.clone();
            (
                RoomId::parse(sanitize_id(&path.to_string_lossy()))
                    .unwrap_or_else(|_| fallback_room()),
                RoomScope::PathPrefix(path.clone()),
                format!("workspace: {}", path.display()),
            )
        }
        (None, true) => (fallback_room(), RoomScope::Global, "global".to_string()),
    };
    Room {
        room_id,
        scope,
        title,
        created_at: now_micros(),
    }
}

/// Derive the canonical repo room for an explicit `(remote, cwd)` the same way the broker's
/// auto-join does. Builds a minimal [`ScopeChain`] (no ancestors / session lineage) and routes it
/// through [`default_room_for`], so the returned room carries the EXACT id / scope / title an agent
/// auto-joins at connect. Used by `get_or_create_chat_room_for_path` so an agent can resolve — and
/// join — another repo's room without re-deriving the keying rules. `cwd` empty ⇒ the global room.
pub(crate) fn repo_room_for(remote: Option<String>, cwd: Option<std::path::PathBuf>) -> Room {
    let chain = ScopeChain {
        remote,
        cwd: cwd.unwrap_or_default(),
        ancestors: Vec::new(),
        session_id: None,
        parent_agent: None,
    };
    default_room_for(&chain)
}

fn fallback_room() -> RoomId {
    RoomId::parse("global").expect("`global` is a valid room id")
}

/// Map an arbitrary string to the id alphabet (`[A-Za-z0-9._:-]`), truncated to the id cap.
pub(super) fn sanitize_id(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.len() > ids::MAX_ID_LEN {
        out.truncate(ids::MAX_ID_LEN);
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// Mint a unique message id from the room, agent, and a microsecond timestamp + a process
/// counter. Collisions are structurally impossible within a single daemon because the counter
/// is monotonic and the daemon is the sole writer.
pub(super) fn mint_message_id(room: &RoomId, agent: &AgentId) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}:{}:{}:{}",
        room.as_str(),
        agent.as_str(),
        now_micros(),
        n
    )
}
