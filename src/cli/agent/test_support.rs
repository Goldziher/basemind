//! What the tests of the `agent` commands share: a parser for the grammar, callers of the
//! conformance suite's repository, and runs of a command line against the in-memory store.

use std::io::Write;
use std::time::Duration;

use clap::Parser;
use serde_json::Value;

use super::args::AgentCmd;
use super::render::Render;
use super::{run_inbox, run_post, run_threads};
use crate::cli::exit::exit_code_for;
use crate::comms::v2::conformance::{Repo, default_caller};
use crate::comms::v2::fake::{FAKE_EPOCH_MICROS, FakeAgents};
use crate::comms::v2::front::Session;
use crate::comms::v2::{Caller, CommsError, InboxBody, InboxReply, Principal, RepoKey};

/// Ninety seconds past the fake's clock, which a post does not move: every age reads `1m`.
const NOW_MICROS: i64 = FAKE_EPOCH_MICROS + 90 * 1_000_000;

/// Text output, with ages counted from [`NOW_MICROS`].
pub const TEXT: Render = Render {
    json: false,
    now_micros: NOW_MICROS,
};

/// JSON output.
pub const JSON: Render = Render {
    json: true,
    now_micros: NOW_MICROS,
};

#[derive(Parser, Debug)]
#[command(name = "agent")]
struct Harness {
    #[command(subcommand)]
    cmd: AgentCmd,
}

/// Parse `args` as what follows `basemind agent`.
pub fn parse(args: &[&str]) -> Result<AgentCmd, clap::Error> {
    Harness::try_parse_from(std::iter::once("agent").chain(args.iter().copied())).map(|harness| harness.cmd)
}

/// The agent `name` in the conformance suite's home repository.
pub fn caller(name: &str) -> Caller {
    default_caller(name, Repo::Home)
}

/// What the binary resolved about the session of one agent: its principal and its repository.
pub struct Owner {
    principal: Principal,
    repo: RepoKey,
}

impl Owner {
    /// The session, with no replay key (a CLI call has none) and no notices of its own.
    pub fn session(&self) -> Session<'_> {
        Session {
            principal: &self.principal,
            repo: &self.repo,
            replay_key: None,
            notices: &[],
        }
    }
}

/// The principal `name` in the conformance suite's home repository.
pub fn owner(name: &str) -> Owner {
    Owner {
        principal: Principal::parse(name).expect("a valid principal"),
        repo: caller(name).repo,
    }
}

/// What `run` wrote.
pub fn capture(run: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>) -> anyhow::Result<String> {
    let mut buffer = Vec::new();
    run(&mut buffer)?;
    Ok(String::from_utf8(buffer).expect("the output is UTF-8"))
}

/// Run the command line `args` as agent `who`. A read that asks for a wait fails: the tests of
/// the wait inject their own.
fn run(fake: &FakeAgents, who: &str, opts: &Render, args: &[&str]) -> anyhow::Result<String> {
    let no_wait = |_: &Caller, _: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        Err(CommsError::Backend {
            detail: "this test injected no wait".to_string(),
        })
    };
    let owner = owner(who);
    let acting = owner.session();
    let cmd = parse(args).expect("the command line parses");
    capture(|out| match cmd {
        AgentCmd::Post(args) => run_post(fake, &acting, args, opts, out),
        AgentCmd::Inbox(args) => run_inbox(fake, &acting, args, &no_wait, opts, out),
        AgentCmd::Threads(args) => run_threads(fake, &acting, args, opts, out),
        AgentCmd::Status | AgentCmd::Cleanup => panic!("status and cleanup run no AgentOps operation"),
    })
}

/// The text output of a command that succeeds.
pub fn text(fake: &FakeAgents, who: &str, args: &[&str]) -> String {
    run(fake, who, &TEXT, args).expect("the command succeeds")
}

/// The `--json` output of a command that succeeds.
pub fn json(fake: &FakeAgents, who: &str, args: &[&str]) -> Value {
    let output = run(fake, who, &JSON, args).expect("the command succeeds");
    serde_json::from_str(&output).expect("the output is JSON")
}

/// The exit code and the message of a command that fails.
pub fn refusal(fake: &FakeAgents, who: &str, args: &[&str]) -> (u8, String) {
    let error = run(fake, who, &TEXT, args).expect_err("the command fails");
    (exit_code_for(&error), error.to_string())
}
