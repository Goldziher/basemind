//! Flat tool arguments to typed requests (ADR-0021).
//!
//! The three agent tools, the CLI and the hooks all receive the same flat fields. Turning them
//! into a request is done here and nowhere else, so every front end refuses the same
//! combinations with the same words:
//!
//! - an empty string for an optional field, or a field passed with its default value, is the
//!   field left out;
//! - a field that does not apply to the call is refused, never ignored, because an ignored
//!   field reads as a call that honoured it;
//! - ids are parsed here, so a malformed one is refused with the value and its grammar.

use std::time::Duration;

use super::api::{
    InboxBody, InboxSelect, Mark, PostTarget, ThreadAction, ThreadFilter, ThreadQuery, ThreadUpdate,
    check_recipient_count, checked_wait, given_limit, normalize_query, normalize_subject, normalize_thread_path,
    normalize_thread_paths,
};
use super::cursor::Cursor;
use super::error::CommsError;
use super::ids::{Actor, MessageId, ThreadId};
use crate::path::RelPath;

/// An optional text field as given, with the empty string read as absent.
fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn parse_actors(ids: &[String]) -> Result<Vec<Actor>, CommsError> {
    ids.iter()
        .map(|id| Actor::parse(id.as_str()).map_err(CommsError::from))
        .collect()
}

/// The names of the fields that are set, in the order given.
fn set_fields<const N: usize>(fields: [(&str, bool); N]) -> Vec<String> {
    fields
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(field, _)| field.to_string())
        .collect()
}

impl PostTarget {
    /// Build the target from `agent_post`'s flat fields, refusing zero or several targets and
    /// refusing `to` / `paths` without `subject` rather than dropping them. The subject, the
    /// recipient count and the paths are checked here; that `to` names someone beside the caller
    /// is checked where the caller is known.
    pub fn from_fields(
        thread: Option<&str>,
        reply_to: Option<&str>,
        subject: Option<&str>,
        to: &[String],
        paths: &[String],
    ) -> Result<Self, CommsError> {
        let (thread, reply_to, subject) = (present(thread), present(reply_to), present(subject));
        let given = set_fields([
            ("thread", thread.is_some()),
            ("reply_to", reply_to.is_some()),
            ("subject", subject.is_some()),
        ]);
        match (thread, reply_to, subject) {
            (None, None, Some(subject)) => {
                let to = parse_actors(to)?;
                check_recipient_count("to", &to)?;
                let paths: Vec<RelPath> = paths.iter().map(RelPath::from).collect();
                Ok(Self::NewThread {
                    subject: normalize_subject(subject)?,
                    to,
                    paths: normalize_thread_paths(&paths)?,
                })
            }
            (Some(_), None, None) | (None, Some(_), None) if !to.is_empty() => Err(CommsError::CreationOnly {
                field: "to".to_string(),
            }),
            (Some(_), None, None) | (None, Some(_), None) if !paths.is_empty() => Err(CommsError::CreationOnly {
                field: "paths".to_string(),
            }),
            (Some(thread), None, None) => Ok(Self::Thread(ThreadId::parse(thread)?)),
            (None, Some(reply_to), None) => Ok(Self::ReplyTo(MessageId::parse(reply_to)?)),
            _ => Err(CommsError::InvalidPostTarget { given }),
        }
    }
}

impl InboxSelect {
    /// Build the selection from `agent_inbox`'s `thread` and `id`. Both together are refused: an
    /// id already names its thread.
    pub fn from_fields(thread: Option<&str>, id: Option<&str>) -> Result<Self, CommsError> {
        match (present(thread), present(id)) {
            (Some(_), Some(_)) => Err(CommsError::not_applicable("thread", "id")),
            (Some(thread), None) => Ok(Self::Thread(ThreadId::parse(thread)?)),
            (None, Some(id)) => Ok(Self::Message(MessageId::parse(id)?)),
            (None, None) => Ok(Self::All),
        }
    }
}

/// `agent_inbox`'s flat fields, as a front end received them. `None` is the field left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxFields {
    /// Read one thread.
    pub thread: Option<String>,
    /// Read one message, whole.
    pub id: Option<String>,
    /// Only unread messages; `true` when left out.
    pub unread: Option<bool>,
    /// Return without marking; `false` when left out.
    pub peek: Option<bool>,
    /// Seconds to wait for a message when nothing is unread; 0 when left out.
    pub wait_secs: Option<u32>,
    /// Page size.
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

/// A checked `agent_inbox` call: the read, and how long the daemon may wait for it to have
/// something to return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxCall {
    /// The read.
    pub body: InboxBody,
    /// The wait, at most [`MAX_WAIT_SECS`](super::api::MAX_WAIT_SECS) seconds. Zero for a read
    /// that does not wait.
    pub wait: Duration,
}

impl InboxCall {
    /// Build the call from the flat fields. Beyond what [`InboxBody::kind`] refuses, a wait is
    /// refused beside `id` and beside `unread: false`: neither read is waiting for anything.
    pub fn from_fields(fields: InboxFields) -> Result<Self, CommsError> {
        let select = InboxSelect::from_fields(fields.thread.as_deref(), fields.id.as_deref())?;
        let unread = fields.unread.unwrap_or(true);
        let wait = checked_wait(fields.wait_secs.unwrap_or(0))?;
        if !wait.is_zero() {
            if matches!(select, InboxSelect::Message(_)) {
                return Err(CommsError::not_applicable("wait_secs", "id"));
            }
            if !unread {
                return Err(CommsError::not_applicable("wait_secs", "unread"));
            }
        }
        let body = InboxBody {
            select,
            unread,
            mark: if fields.peek.unwrap_or(false) {
                Mark::Peek
            } else {
                Mark::Read
            },
            limit: fields.limit,
            cursor: present(fields.cursor.as_deref()).map(Cursor::from_wire),
        };
        body.kind()?;
        Ok(Self { body, wait })
    }
}

/// `agent_threads`' flat fields, as a front end received them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadsFields {
    /// Subject substring to discover threads by.
    pub query: Option<String>,
    /// Repository-relative path to discover threads by.
    pub path: Option<String>,
    /// The thread to describe or change.
    pub thread: Option<String>,
    /// Join `thread`.
    pub join: bool,
    /// Leave `thread`.
    pub leave: bool,
    /// Invite these agent ids into `thread`.
    pub invite: Option<Vec<String>>,
    /// Archive `thread`.
    pub archive: bool,
    /// Include archived threads in a listing.
    pub archived: bool,
    /// Page size of a listing.
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page of a listing.
    pub cursor: Option<String>,
}

/// A checked `agent_threads` call: a query or an update, never both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadsCall {
    /// List, discover or describe.
    Query(ThreadQuery),
    /// Join, leave, invite or archive.
    Update(ThreadUpdate),
}

impl ThreadsCall {
    /// Build the call from the flat fields.
    ///
    /// More than one of `join`, `leave`, `invite`, `archive` is refused naming them. One of them
    /// needs `thread`, and is refused beside any listing field (`query`, `path`, `cursor`,
    /// `archived`, `limit`). `thread` alone describes that thread and is refused beside the same
    /// listing fields. Anything else is a listing.
    pub fn from_fields(fields: ThreadsFields) -> Result<Self, CommsError> {
        let actions = set_fields([
            ("join", fields.join),
            ("leave", fields.leave),
            ("invite", fields.invite.is_some()),
            ("archive", fields.archive),
        ]);
        if actions.len() > 1 {
            return Err(CommsError::MultipleUpdates { fields: actions });
        }
        let query = present(fields.query.as_deref());
        let path = present(fields.path.as_deref());
        let cursor = present(fields.cursor.as_deref());
        let thread = present(fields.thread.as_deref()).map(ThreadId::parse).transpose()?;
        let listing_fields = set_fields([
            ("query", query.is_some()),
            ("path", path.is_some()),
            ("cursor", cursor.is_some()),
            ("archived", fields.archived),
            ("limit", given_limit(fields.limit).is_some()),
        ]);
        let refuse_listing_fields = |with: &str| match listing_fields.first() {
            Some(field) => Err(CommsError::not_applicable(field, with)),
            None => Ok(()),
        };

        if let Some(action) = actions.first() {
            let Some(thread) = thread else {
                return Err(CommsError::MissingField {
                    field: "thread".to_string(),
                    needed_by: action.clone(),
                });
            };
            refuse_listing_fields(action)?;
            let action = match fields.invite {
                Some(invited) if invited.is_empty() => return Err(CommsError::EmptyInvite),
                Some(invited) => {
                    let invited = parse_actors(&invited)?;
                    check_recipient_count("invite", &invited)?;
                    ThreadAction::Invite(invited)
                }
                None if fields.join => ThreadAction::Join,
                None if fields.leave => ThreadAction::Leave,
                None => ThreadAction::Archive,
            };
            return Ok(Self::Update(ThreadUpdate { thread, action }));
        }
        if let Some(thread) = thread {
            refuse_listing_fields("thread")?;
            return Ok(Self::Query(ThreadQuery::Describe(thread)));
        }
        if let Some(query) = query {
            normalize_query(query)?;
        }
        Ok(Self::Query(ThreadQuery::List(ThreadFilter {
            query: query.map(|query| query.trim().to_string()),
            path: path
                .map(|path| normalize_thread_path(&RelPath::from(path)))
                .transpose()?,
            archived: fields.archived,
            limit: fields.limit,
            cursor: cursor.map(Cursor::from_wire),
        })))
    }
}

#[cfg(test)]
#[path = "fields_tests.rs"]
mod tests;
