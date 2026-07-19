//! Thread-derivation helpers for the broker, split out of `daemon.rs` to keep it under the
//! 1000-line module cap. These are the id-minting, scope-chain, and dimension-validation rules
//! the broker's `thread_start` routes through.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::comms::ids::{self, AgentId, ThreadId};
use crate::comms::model::now_micros;
use crate::comms::scope::{self, ScopeChain};

/// Build a scope chain from the optional remote + cwd a client supplied. When `cwd` is given we
/// attempt git discovery to enrich the chain's remote if the client did not supply one.
pub(crate) fn build_chain(remote: Option<String>, cwd: Option<std::path::PathBuf>) -> ScopeChain {
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
        },
    }
}

/// A thread must be addressed by AT LEAST TWO of subject / path / members. The creator is always
/// an implicit member, so the `members` dimension counts when there is at least one EXPLICIT extra
/// member (a thread with only the creator is not "addressed by members"). Returns `Ok(())` when
/// the two-of-three rule holds, else a human-readable rejection reason.
pub(crate) fn validate_dimensions(
    subject: Option<&str>,
    path: Option<&str>,
    members: &[AgentId],
    creator: &AgentId,
) -> Result<(), String> {
    let has_subject = subject.is_some_and(|s| !s.is_empty());
    let has_path = path.is_some_and(|p| !p.is_empty());
    let has_members = members.iter().any(|m| m != creator);
    let count = [has_subject, has_path, has_members].iter().filter(|b| **b).count();
    if count >= 2 {
        Ok(())
    } else {
        Err(format!(
            "a thread must be addressed by at least 2 of subject / path / members (got \
             subject={has_subject}, path={has_path}, members={has_members}); supply at least two"
        ))
    }
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

/// Mint a unique thread id from the creator, a microsecond timestamp, and a process counter.
/// Collisions are structurally impossible within a single daemon because the counter is monotonic
/// and the daemon is the sole writer.
pub(crate) fn mint_thread_id(creator: &AgentId) -> ThreadId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let raw = format!("th-{}-{}-{}", sanitize_id(creator.as_str()), now_micros(), n);
    ThreadId::parse(sanitize_id(&raw))
        .unwrap_or_else(|_| ThreadId::parse(format!("th-{n}")).expect("`th-<n>` is a valid thread id"))
}

/// Mint a unique message id from the thread, agent, and a microsecond timestamp + a process
/// counter.
pub(crate) fn mint_message_id(thread: &ThreadId, agent: &AgentId) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}:{}:{}:{}", thread.as_str(), agent.as_str(), now_micros(), n)
}
