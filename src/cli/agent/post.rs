//! `basemind agent post` — the CLI half of `agent_post`.

use std::io::Write;

use anyhow::Result;

use super::args::PostArgs;
use super::render::{self, Render};
use super::{caller, failed, refused};
use crate::comms::v2::front::{PostFields, Session, post_body};
use crate::comms::v2::{AgentOps, CommsError, PostBody};

/// The command's name in an error message.
const COMMAND: &str = "agent post";

/// The post `args` ask for. Refuses what `agent_post` refuses: an empty or oversized body, no
/// target or several, `--to` / `--paths` without `--subject`, a malformed id.
pub fn post_request(args: PostArgs) -> Result<PostBody, CommsError> {
    post_body(PostFields {
        body: args.body,
        thread: args.thread,
        reply_to: args.reply_to,
        subject: args.subject,
        to: args.to,
        paths: args.paths,
    })
}

/// Post a message and print its id.
pub fn run_post(
    ops: &dyn AgentOps,
    session: &Session<'_>,
    mut args: PostArgs,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    let caller = caller(session, COMMAND, args.as_name.take().as_deref())?;
    let body = post_request(args).map_err(|error| refused(COMMAND, &error))?;
    let mut reply = ops.post(&caller, body).map_err(|error| failed(COMMAND, &error))?;
    reply.notices = session.notices(std::mem::take(&mut reply.notices));
    render::post(&reply, opts, out)
}

#[cfg(test)]
#[path = "post_tests.rs"]
mod tests;
