//! `basemind agent` in the binary: resolve who is calling, connect to the daemon, run one command.
//!
//! Identity comes from the one resolver `serve` and the hooks use
//! ([`resolve_for_root`](crate::comms::v2::identity::resolve_for_root)), over the root the binary
//! discovered, so a hook started in a subdirectory acts as its `serve` session.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::args::{AgentCmd, CleanupArgs};
use super::render::Render;
use super::{hook, run_inbox, run_post, run_report, run_threads};
use crate::comms::agents_hub::{AgentCleanupReport, AgentStoreStatus};
use crate::comms::client::{ClientHello, CommsClient};
use crate::comms::client_agents::RemoteAgents;
use crate::comms::v2::front::Session;
use crate::comms::v2::identity::{self, HOST_ENV, ResolvedIdentity};
use crate::comms::v2::{CommsError, RepoKey, ThreadId};

/// What `basemind agent status` prints: who this checkout acts as, and the daemon's store.
#[derive(Serialize)]
struct StatusReport {
    principal: String,
    identity_from: &'static str,
    host: Option<String>,
    root: String,
    store: AgentStoreStatus,
}

/// Run one `basemind agent` command for the checkout at `root`.
pub fn run(root: &Path, json: bool, cmd: AgentCmd) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if let AgentCmd::Hook { event } = cmd {
        return hook::run(event, &mut std::io::stdin().lock(), &mut out);
    }
    let resolved = identity::resolve_for_root(root).context("resolve this checkout's agent identity")?;
    let canonical = identity::canonical_root(root).context("resolve the checkout root")?;
    let host = std::env::var(HOST_ENV).ok().filter(|host| !host.is_empty());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let hello = ClientHello::agent(
        resolved.principal.clone(),
        canonical.as_path().to_path_buf(),
        host.clone(),
    );
    let client = runtime
        .block_on(CommsClient::ensure_and_connect(hello))
        .map_err(|error| anyhow::anyhow!("connect to the basemind daemon: {error}"))?;
    let remote = RemoteAgents::new(runtime, client, resolved.principal.as_str().to_string());
    let repo = RepoKey::from_root(canonical.as_path()).context("name the checkout")?;
    let notices = resolved.notices();
    let session = Session {
        principal: &resolved.principal,
        repo: &repo,
        replay_key: None,
        notices: &notices,
    };
    let opts = Render::now(json);
    dispatch(
        &remote,
        &session,
        &resolved,
        (canonical.as_path(), host),
        cmd,
        &opts,
        &mut out,
    )
}

fn dispatch(
    remote: &RemoteAgents,
    session: &Session<'_>,
    resolved: &ResolvedIdentity,
    (root, host): (&Path, Option<String>),
    cmd: AgentCmd,
    opts: &Render,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        AgentCmd::Post(args) => run_post(remote, session, args, opts, out),
        AgentCmd::Inbox(args) => {
            let wait = |caller: &crate::comms::v2::Caller, body, wait| remote.waiting_read(caller, body, wait);
            run_inbox(remote, session, args, &wait, opts, out)
        }
        AgentCmd::Threads(args) => run_threads(remote, session, args, opts, out),
        AgentCmd::Status => {
            let report = remote.status().map(|store| StatusReport {
                principal: resolved.principal.as_str().to_string(),
                identity_from: resolved.tier.label(),
                host,
                root: root.display().to_string(),
                store,
            });
            run_report("agent status", report, opts, out)
        }
        AgentCmd::Cleanup(CleanupArgs { archive }) => {
            let report: Result<AgentCleanupReport, CommsError> = archive
                .iter()
                .map(|thread| ThreadId::parse(thread).map_err(CommsError::from))
                .collect::<Result<Vec<_>, _>>()
                .and_then(|threads| remote.cleanup(threads));
            run_report("agent cleanup", report, opts, out)
        }
        AgentCmd::Hook { .. } => Ok(()),
    }
}
