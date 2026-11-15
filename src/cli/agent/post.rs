//! `basemind agent post` — the CLI half of `agent_post`.

use std::io::Write;

use anyhow::Result;

use super::args::PostArgs;
use super::render::{self, Render};
use super::{acting_as, failed, refused};
use crate::comms::v2::api::check_body;
use crate::comms::v2::{AgentOps, Caller, CommsError, PostBody, PostTarget};

/// The command's name in an error message.
const COMMAND: &str = "agent post";

/// The post `args` ask for and the caller making it. Refuses what `agent_post` refuses: no
/// target or several, `--to` / `--paths` without `--subject`, a malformed id, an empty body.
pub fn post_request(caller: &Caller, args: PostArgs) -> Result<(Caller, PostBody), CommsError> {
    let caller = acting_as(caller, args.as_name.as_deref())?;
    let target = PostTarget::from_fields(
        args.thread.as_deref(),
        args.reply_to.as_deref(),
        args.subject.as_deref(),
        &args.to,
        &args.paths,
    )?;
    check_body(&args.body)?;
    Ok((
        caller,
        PostBody {
            target,
            body: args.body,
        },
    ))
}

/// Post a message and print its id.
pub fn run_post(ops: &dyn AgentOps, caller: &Caller, args: PostArgs, opts: &Render, out: &mut dyn Write) -> Result<()> {
    let (caller, body) = post_request(caller, args).map_err(|error| refused(COMMAND, &error))?;
    let reply = ops.post(&caller, body).map_err(|error| failed(COMMAND, &error))?;
    render::post(&reply, opts, out)
}

#[cfg(test)]
#[path = "post_tests.rs"]
mod tests;
