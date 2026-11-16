//! The broker's agent-comms v2 handlers (ADR-0021): `Hello`, the four agent requests, and the
//! operator's status and cleanup, over the [`AgentHub`](super::agents_hub::AgentHub).
//!
//! Who calls is the link's: the principal and root its `Hello` named, and the sub-name the call
//! carries. Nothing in a request body names the sender.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::daemon::{Broker, Session};
use super::protocol::{CommsResponse, PROTO_VER};
use super::v2::api::{
    Actor, Caller, CommsError, InboxBody, PostBody, Principal, SubName, ThreadId, ThreadQuery, ThreadUpdate,
    checked_wait,
};

/// The refusal of an agent request on a link whose `Hello` named no principal or no root.
fn anonymous_link() -> CommsError {
    CommsError::Backend {
        detail: "this connection's hello named no agent and no checkout root: connect with an identity to use agent \
                 requests"
            .to_string(),
    }
}

fn refused(error: CommsError) -> CommsResponse {
    CommsResponse::AgentRefused(error)
}

impl Broker {
    /// Negotiate the protocol and fix the link's identity. The repository of `root` is derived
    /// here, once per link.
    pub(super) async fn on_hello(
        &self,
        proto_ver: u32,
        principal: Option<Principal>,
        root: Option<PathBuf>,
        host: Option<String>,
        session: &mut Session,
    ) -> CommsResponse {
        if proto_ver != PROTO_VER {
            return CommsResponse::Error {
                code: "proto_skew".to_string(),
                message: format!("daemon speaks proto {PROTO_VER}, client sent {proto_ver}"),
            };
        }
        if let Some(root) = &root {
            let (hub, at) = (Arc::clone(&self.agents), root.clone());
            session.repo = tokio::task::spawn_blocking(move || hub.repo_for(&at))
                .await
                .ok()
                .and_then(Result::ok);
            let target = root.clone();
            if let Err(error) = self.registry_blocking(move |reg| reg.register_workspace(&target)).await {
                tracing::warn!(%error, root = %root.display(), "comms: registry auto-register failed");
            }
        }
        session.principal = principal;
        session.root = root;
        session.host = host;
        CommsResponse::Welcome {
            proto_ver: PROTO_VER,
            daemon_version: self.version.clone(),
        }
    }

    /// The caller of an agent request on this link, acting as `sub` when one is given.
    fn link_caller(session: &Session, sub: Option<SubName>) -> Result<Caller, CommsError> {
        let (Some(principal), Some(repo)) = (&session.principal, &session.repo) else {
            return Err(anonymous_link());
        };
        Ok(Caller::new(Actor::new(principal, sub.as_ref())?, repo.clone()))
    }

    pub(super) async fn on_agent_post(&self, session: &Session, sub: Option<SubName>, body: PostBody) -> CommsResponse {
        let caller = match Self::link_caller(session, sub) {
            Ok(caller) => caller,
            Err(error) => return refused(error),
        };
        match self.agents.post(caller, body).await {
            Ok(reply) => CommsResponse::AgentPosted(reply),
            Err(error) => refused(error),
        }
    }

    /// An inbox read, waiting up to `wait_secs`. `gone` is cancelled when the client hangs up.
    pub(super) async fn on_agent_inbox(
        &self,
        session: &Session,
        sub: Option<SubName>,
        body: InboxBody,
        wait_secs: u32,
        gone: &CancellationToken,
    ) -> CommsResponse {
        let read = async {
            let caller = Self::link_caller(session, sub)?;
            let wait = checked_wait(wait_secs)?;
            self.agents.read(caller, body, wait, gone).await
        };
        match read.await {
            Ok(reply) => CommsResponse::AgentRead(reply),
            Err(error) => refused(error),
        }
    }

    pub(super) async fn on_agent_threads(
        &self,
        session: &Session,
        sub: Option<SubName>,
        query: ThreadQuery,
    ) -> CommsResponse {
        let caller = match Self::link_caller(session, sub) {
            Ok(caller) => caller,
            Err(error) => return refused(error),
        };
        match self.agents.threads_query(caller, query).await {
            Ok(reply) => CommsResponse::AgentThreads(reply),
            Err(error) => refused(error),
        }
    }

    pub(super) async fn on_agent_thread_update(
        &self,
        session: &Session,
        sub: Option<SubName>,
        update: ThreadUpdate,
    ) -> CommsResponse {
        let caller = match Self::link_caller(session, sub) {
            Ok(caller) => caller,
            Err(error) => return refused(error),
        };
        match self.agents.threads_update(caller, update).await {
            Ok(reply) => CommsResponse::AgentThreads(reply),
            Err(error) => refused(error),
        }
    }

    pub(super) fn on_agent_status(&self, session: &Session) -> CommsResponse {
        let principal = session.principal.as_ref().map(Principal::as_str);
        CommsResponse::AgentStatus(self.agents.status(principal, session.repo.clone()))
    }

    /// The agent store's periodic sweep and flush: the maintenance tick and the shutdown.
    pub async fn maintain_agents(&self) {
        self.agents.maintain().await;
    }

    pub(super) async fn on_agent_cleanup(&self, archive: Vec<ThreadId>) -> CommsResponse {
        let _working = self.begin_work();
        match self.agents.cleanup(archive).await {
            Ok(report) => CommsResponse::AgentCleaned(report),
            Err(error) => refused(error),
        }
    }
}
