//! `basemind agent threads` — the CLI half of `agent_threads`.

use std::io::Write;

use anyhow::Result;

use super::args::ThreadsArgs;
use super::render::{self, Render, ThreadsView};
use super::{caller, failed, refused};
use crate::comms::v2::front::Session;
use crate::comms::v2::{AgentOps, CommsError, ThreadAction, ThreadQuery, ThreadsCall, ThreadsFields};

/// The command's name in an error message.
const COMMAND: &str = "agent threads";

/// The query or change `args` ask for. Refuses what `agent_threads` refuses: several changes, a
/// change without `--thread`, a listing flag beside a change or a thread to describe.
pub fn threads_request(args: ThreadsArgs) -> Result<ThreadsCall, CommsError> {
    let invite = (!args.invite.is_empty()).then_some(args.invite);
    ThreadsCall::from_fields(ThreadsFields {
        query: args.query,
        path: args.path,
        thread: args.thread,
        join: args.join,
        leave: args.leave,
        invite,
        archive: args.archive,
        archived: args.archived,
        limit: args.limit,
        cursor: args.cursor,
    })
}

/// What a change is reported as, before the id of the thread it changed.
fn change_label(action: &ThreadAction) -> &'static str {
    match action {
        ThreadAction::Join => "joined",
        ThreadAction::Leave => "left",
        ThreadAction::Invite(_) => "invited into",
        ThreadAction::Archive => "archived",
    }
}

/// List, discover or describe threads, or change one, and print the threads concerned.
pub fn run_threads(
    ops: &dyn AgentOps,
    session: &Session<'_>,
    mut args: ThreadsArgs,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    let caller = caller(session, COMMAND, args.as_name.take().as_deref())?;
    let call = threads_request(args).map_err(|error| refused(COMMAND, &error))?;
    let (view, reply) = match call {
        ThreadsCall::Query(query) => {
            let view = match &query {
                ThreadQuery::List(_) => ThreadsView::List,
                ThreadQuery::Describe(_) => ThreadsView::Describe,
            };
            (view, ops.threads_query(&caller, query))
        }
        ThreadsCall::Update(update) => {
            let view = ThreadsView::Changed(change_label(&update.action));
            (view, ops.threads_update(&caller, update))
        }
    };
    let mut reply = reply.map_err(|error| failed(COMMAND, &error))?;
    reply.notices = session.notices(std::mem::take(&mut reply.notices));
    render::threads(&reply, view, opts, out)
}

#[cfg(test)]
#[path = "threads_tests.rs"]
mod tests;
