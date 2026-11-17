//! The agent requests of a [`CommsClient`], and [`RemoteAgents`]: the daemon's agent store as a
//! synchronous [`AgentOps`] for the CLI and the hooks.
//!
//! A link speaks for the principal its `Hello` named; a call chooses only the sub-identity. The
//! [`Caller`] an [`AgentOps`] call carries is therefore reduced to its sub-name here, and a caller
//! of another principal is refused before anything is sent.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::agents_hub::{AgentCleanupReport, AgentStoreStatus};
use super::client::{CommsClient, CommsClientError};
use super::protocol::{CommsRequest, CommsResponse};
use super::v2::api::{
    AgentOps, Caller, CommsError, InboxBody, InboxReply, PostBody, PostReply, SubName, ThreadId, ThreadQuery,
    ThreadUpdate, ThreadsReply,
};

impl CommsClient {
    fn agent_reply(&self, response: CommsResponse, request: &'static str) -> Result<CommsResponse, CommsClientError> {
        match response {
            CommsResponse::AgentRefused(error) => Err(CommsClientError::Agent(error)),
            CommsResponse::Error { .. } => Err(self.shape_err(response, request)),
            other => Ok(other),
        }
    }

    /// `agent_post` as the link's principal, or its sub-identity `sub`.
    pub async fn agent_post(&mut self, sub: Option<SubName>, body: PostBody) -> Result<PostReply, CommsClientError> {
        match self.request(CommsRequest::AgentPost { sub, body }).await? {
            CommsResponse::AgentPosted(reply) => Ok(reply),
            other => Err(self.unexpected(other, "agent_post")),
        }
    }

    /// `agent_inbox`, waiting in the daemon up to `wait_secs` while nothing is unread.
    pub async fn agent_inbox(
        &mut self,
        sub: Option<SubName>,
        body: InboxBody,
        wait_secs: u32,
    ) -> Result<InboxReply, CommsClientError> {
        match self.request(CommsRequest::AgentInbox { sub, body, wait_secs }).await? {
            CommsResponse::AgentRead(reply) => Ok(reply),
            other => Err(self.unexpected(other, "agent_inbox")),
        }
    }

    /// `agent_threads` without a change.
    pub async fn agent_threads(
        &mut self,
        sub: Option<SubName>,
        query: ThreadQuery,
    ) -> Result<ThreadsReply, CommsClientError> {
        match self.request(CommsRequest::AgentThreads { sub, query }).await? {
            CommsResponse::AgentThreads(reply) => Ok(reply),
            other => Err(self.unexpected(other, "agent_threads")),
        }
    }

    /// `agent_threads` with a change.
    pub async fn agent_thread_update(
        &mut self,
        sub: Option<SubName>,
        update: ThreadUpdate,
    ) -> Result<ThreadsReply, CommsClientError> {
        match self.request(CommsRequest::AgentThreadUpdate { sub, update }).await? {
            CommsResponse::AgentThreads(reply) => Ok(reply),
            other => Err(self.unexpected(other, "agent_thread_update")),
        }
    }

    /// The agent store's state, and the link's repository.
    pub async fn agent_status(&mut self) -> Result<AgentStoreStatus, CommsClientError> {
        match self.request(CommsRequest::AgentStatus).await? {
            CommsResponse::AgentStatus(status) => Ok(status),
            other => Err(self.unexpected(other, "agent_status")),
        }
    }

    /// Archive `archive` whoever created them, then sweep once.
    pub async fn agent_cleanup(&mut self, archive: Vec<ThreadId>) -> Result<AgentCleanupReport, CommsClientError> {
        match self.request(CommsRequest::AgentCleanup { archive }).await? {
            CommsResponse::AgentCleaned(report) => Ok(report),
            other => Err(self.unexpected(other, "agent_cleanup")),
        }
    }

    fn unexpected(&self, response: CommsResponse, request: &'static str) -> CommsClientError {
        match self.agent_reply(response, request) {
            Err(error) => error,
            Ok(_) => CommsClientError::Unexpected { request },
        }
    }
}

/// A client failure as the refusal an agent surface shows. A daemon refusal arrives as itself.
pub fn as_comms_error(error: CommsClientError) -> CommsError {
    match error {
        CommsClientError::Agent(error) => error,
        other => CommsError::Backend {
            detail: format!("the basemind daemon could not be asked: {other}"),
        },
    }
}

/// The daemon's agent store over one link, as a synchronous [`AgentOps`]: what the CLI commands
/// run against. Each call blocks on the link's runtime.
pub struct RemoteAgents {
    runtime: tokio::runtime::Runtime,
    client: Mutex<CommsClient>,
    principal: String,
}

impl RemoteAgents {
    /// Wrap `client`, connected on `runtime` with a `Hello` naming `principal`.
    pub fn new(runtime: tokio::runtime::Runtime, client: CommsClient, principal: String) -> Self {
        Self {
            runtime,
            client: Mutex::new(client),
            principal,
        }
    }

    fn sub(&self, caller: &Caller) -> Result<Option<SubName>, CommsError> {
        if caller.actor.principal() != self.principal {
            return Err(CommsError::Backend {
                detail: format!(
                    "this link speaks for {}, not {}",
                    self.principal,
                    caller.actor.principal()
                ),
            });
        }
        Ok(caller.actor.sub().map(SubName::parse).transpose()?)
    }

    fn run<T>(&self, call: impl AsyncFnOnce(&mut CommsClient) -> Result<T, CommsClientError>) -> Result<T, CommsError> {
        let mut client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        self.runtime.block_on(call(&mut client)).map_err(as_comms_error)
    }

    /// A read that waits in the daemon up to `wait`.
    pub fn waiting_read(&self, caller: &Caller, body: InboxBody, wait: Duration) -> Result<InboxReply, CommsError> {
        let sub = self.sub(caller)?;
        let secs = u32::try_from(wait.as_secs()).unwrap_or(u32::MAX);
        self.run(async move |client| client.agent_inbox(sub, body, secs).await)
    }

    /// The agent store's state.
    pub fn status(&self) -> Result<AgentStoreStatus, CommsError> {
        self.run(async |client| client.agent_status().await)
    }

    /// Archive `threads` whoever created them, then sweep once.
    pub fn cleanup(&self, threads: Vec<ThreadId>) -> Result<AgentCleanupReport, CommsError> {
        self.run(async move |client| client.agent_cleanup(threads).await)
    }
}

impl AgentOps for RemoteAgents {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        let sub = self.sub(caller)?;
        self.run(async move |client| client.agent_post(sub, body).await)
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let sub = self.sub(caller)?;
        self.run(async move |client| client.agent_inbox(sub, body, 0).await)
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        let sub = self.sub(caller)?;
        self.run(async move |client| client.agent_threads(sub, query).await)
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        let sub = self.sub(caller)?;
        self.run(async move |client| client.agent_thread_update(sub, update).await)
    }

    fn arrivals(&self) -> u64 {
        0
    }
}
