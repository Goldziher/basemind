//! What the agent tools and the `basemind agent` commands do the same way (ADR-0021).
//!
//! A front end holds a [`Session`], turns its flat arguments into a request here, and words a
//! refusal here, so the two cannot drift apart. Nothing in this module knows MCP or clap.

use std::time::Duration;

use super::api::{
    AgentOps, Caller, CommsError, InboxBody, InboxCall, InboxFields, InboxReply, Mark, PostBody, PostTarget, Principal,
    ReplayKey, RepoKey, check_body,
};
use super::identity::compose;

/// The tool field and CLI flag that asks for the announcing read.
pub const ANNOUNCE_FIELD: &str = "announce";

/// What a front end holds about the session a call arrives on. None of it is read from a request.
#[derive(Clone, Copy, Debug)]
pub struct Session<'a> {
    /// The identity the session resolved to.
    pub principal: &'a Principal,
    /// The repository the session works in.
    pub repo: &'a RepoKey,
    /// The relay's key for this request, when it came through a relay.
    pub replay_key: Option<&'a ReplayKey>,
    /// Notices only the front end knows, appended to every reply after the ones it carries: the
    /// rejected-id notice on the session's first reply, the shared-identity notice while it holds.
    pub notices: &'a [String],
}

impl Session<'_> {
    /// The caller of one call: the session's principal, or its sub-identity `as_`. An empty `as_`
    /// is the field left out.
    pub fn caller(&self, as_: Option<&str>) -> Result<Caller, CommsError> {
        let caller = Caller::new(compose(self.principal, as_)?, self.repo.clone());
        Ok(match self.replay_key {
            Some(key) => caller.with_replay_key(key.clone()),
            None => caller,
        })
    }

    /// The notices of a reply: the ones it came with, unchanged, then the session's.
    pub fn notices(&self, mut from_reply: Vec<String>) -> Vec<String> {
        from_reply.extend(self.notices.iter().cloned());
        from_reply
    }
}

/// `agent_post`'s flat fields, as a front end received them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostFields {
    /// The message text.
    pub body: String,
    /// Post into this thread.
    pub thread: Option<String>,
    /// Reply to this message, in its thread.
    pub reply_to: Option<String>,
    /// Create a thread with this subject.
    pub subject: Option<String>,
    /// Members of the new thread beside its creator.
    pub to: Vec<String>,
    /// Path prefixes the new thread is about.
    pub paths: Vec<String>,
}

/// The post `fields` ask for: the body checked first, then the target. That `to` names someone
/// beside the caller is left to the store, which knows the caller.
pub fn post_body(fields: PostFields) -> Result<PostBody, CommsError> {
    check_body(&fields.body)?;
    let target = PostTarget::from_fields(
        fields.thread.as_deref(),
        fields.reply_to.as_deref(),
        fields.subject.as_deref(),
        &fields.to,
        &fields.paths,
    )?;
    Ok(PostBody {
        target,
        body: fields.body,
    })
}

/// The read `fields` ask for, as the announcing read when `announce` is set.
///
/// An announcing read returns front matter of what the actor has not been told about and moves
/// only the announcement mark. It is refused beside `id`, `peek`, `unread: false` and a wait,
/// each of which asks for another read, and takes no cursor.
pub fn inbox_call(fields: InboxFields, announce: bool) -> Result<InboxCall, CommsError> {
    if !announce {
        return InboxCall::from_fields(fields);
    }
    let conflicts = [
        ("id", fields.id.as_deref().is_some_and(|id| !id.is_empty())),
        ("peek", fields.peek == Some(true)),
        ("unread", fields.unread == Some(false)),
        ("wait_secs", fields.wait_secs.is_some_and(|secs| secs > 0)),
    ];
    if let Some((field, _)) = conflicts.into_iter().find(|(_, set)| *set) {
        return Err(CommsError::not_applicable(field, ANNOUNCE_FIELD));
    }
    let mut call = InboxCall::from_fields(fields)?;
    call.body.mark = Mark::Notify;
    call.body.kind()?;
    Ok(call)
}

/// A read that waits: the daemon's side of `wait_secs`.
///
/// [`AgentOps::inbox`] never blocks and a front end neither sleeps nor spawns. A call that asks to
/// wait is handed over whole, with its wait (above zero, at most 40 seconds), to the daemon's
/// waiting read, which registers the replay key as in flight, watches [`AgentOps::arrivals`],
/// checks that the client is still connected before a read that marks, and sets
/// [`InboxReply::wait`].
pub type WaitingRead<'a> = dyn Fn(&Caller, InboxBody, Duration) -> Result<InboxReply, CommsError> + 'a;

/// Make the read `call` asks for: at once through `ops`, or through `waiting_read` when it asks
/// for a wait.
pub fn inbox_read(
    ops: &dyn AgentOps,
    caller: &Caller,
    call: InboxCall,
    waiting_read: &WaitingRead<'_>,
) -> Result<InboxReply, CommsError> {
    if call.wait.is_zero() {
        ops.inbox(caller, call.body)
    } else {
        waiting_read(caller, call.body, call.wait)
    }
}

/// A refusal as a front end shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The surface that refused, then the refusal's own words, which already say what to do next.
    pub message: String,
    /// Whether the store failed or cannot be written, rather than the request being wrong.
    pub store_failed: bool,
}

/// Word `error` for the user of `surface`, the tool or command that refused.
pub fn refusal(surface: &str, error: &CommsError) -> Refusal {
    Refusal {
        message: format!("{surface}: {error}"),
        store_failed: matches!(error, CommsError::Backend { .. } | CommsError::StoreNewer { .. }),
    }
}

/// Word a refused `as`, which no request field of the store's names.
pub fn as_refusal(surface: &str, error: &CommsError) -> Refusal {
    Refusal {
        message: format!("{surface}: `as`: {error}"),
        store_failed: false,
    }
}

#[cfg(test)]
#[path = "front_tests.rs"]
mod tests;
