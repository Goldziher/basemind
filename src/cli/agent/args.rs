//! The clap grammar of `basemind agent`.
//!
//! A flag is the tool field of the same name with `-` for `_`. Combinations are not constrained
//! here: [`crate::comms::v2::fields`] refuses them, so the CLI and the tools word a refusal alike.
//! `--json` is the binary's global flag and is not repeated here.

use clap::{Args, Subcommand};

/// The agent-coordination commands: one per `agent_*` tool, plus the two with no tool.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AgentCmd {
    /// Post a message: into a thread, as a reply, or as the first message of a new thread.
    Post(PostArgs),
    /// Read messages, bodies inline. Marks what it returns as read unless `--peek` or
    /// `--announce` is set.
    Inbox(InboxArgs),
    /// List, discover or describe threads, or make one change to a thread.
    Threads(ThreadsArgs),
    /// Report the state of the comms store (CLI only).
    Status,
    /// Remove what retention no longer keeps (CLI only).
    Cleanup,
}

/// `agent_post`'s fields.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct PostArgs {
    /// The message text.
    #[arg(long, allow_hyphen_values = true)]
    pub body: String,
    /// Post into this thread. Exactly one of `--thread`, `--reply-to`, `--subject`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Reply to this message, in its thread.
    #[arg(long, value_name = "MESSAGE")]
    pub reply_to: Option<String>,
    /// Start a new thread with this subject.
    #[arg(long)]
    pub subject: Option<String>,
    /// Member of the new thread (repeatable). Any makes the thread invitation-only.
    #[arg(long, value_name = "AGENT")]
    pub to: Vec<String>,
    /// Repository-relative path prefix the new thread is about (repeatable).
    #[arg(long, value_name = "PATH")]
    pub paths: Vec<String>,
    /// Act as this sub-identity of the session's agent.
    #[arg(long = "as", value_name = "NAME")]
    pub as_name: Option<String>,
}

/// `agent_inbox`'s fields.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxArgs {
    /// Read one thread.
    #[arg(long)]
    pub thread: Option<String>,
    /// Read one message, with its whole body.
    #[arg(long, value_name = "MESSAGE")]
    pub id: Option<String>,
    /// Only unread messages (the default). `false` reads history and marks nothing.
    #[arg(long, value_name = "BOOL")]
    pub unread: Option<bool>,
    /// Return unread messages without marking them as read.
    #[arg(long)]
    pub peek: bool,
    /// The hooks' read: list unread messages not announced yet, without bodies, and record them
    /// as announced. Marks nothing as read.
    #[arg(long)]
    pub announce: bool,
    /// Wait up to this many seconds (0-40) for a message when nothing is unread.
    #[arg(long, value_name = "SECONDS")]
    pub wait_secs: Option<u32>,
    /// Most messages to return (default 20, at most 100).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Resume token: the previous page's `next_cursor`. Only with `--peek` or `--unread false`.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Act as this sub-identity of the session's agent.
    #[arg(long = "as", value_name = "NAME")]
    pub as_name: Option<String>,
}

/// `agent_threads`' fields.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadsArgs {
    /// Discover threads whose subject contains this (at least three characters).
    #[arg(long)]
    pub query: Option<String>,
    /// Discover threads about this repository-relative path.
    #[arg(long)]
    pub path: Option<String>,
    /// The thread to describe, or to change with one of the four flags below.
    #[arg(long)]
    pub thread: Option<String>,
    /// Join `--thread`.
    #[arg(long)]
    pub join: bool,
    /// Leave `--thread`.
    #[arg(long)]
    pub leave: bool,
    /// Invite this agent into `--thread` (repeatable).
    #[arg(long, value_name = "AGENT")]
    pub invite: Vec<String>,
    /// Archive `--thread`. Creator only.
    #[arg(long)]
    pub archive: bool,
    /// Include archived threads in a listing.
    #[arg(long)]
    pub archived: bool,
    /// Most threads to return (default 20, at most 100).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Resume token: the previous page's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Act as this sub-identity of the session's agent.
    #[arg(long = "as", value_name = "NAME")]
    pub as_name: Option<String>,
}

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
