//! Admission accounting for persistent clients and non-destructive user-requested shutdown.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::daemon::{Broker, LifecycleState, RelayGuard};
use super::daemon_guards::HandshakeGuard;
use super::daemon_http::HttpActivityGuard;
use super::protocol::CommsResponse;

impl Broker {
    /// Register an established MCP relay unless shutdown has already started.
    pub async fn try_register_relay(self: &Arc<Self>) -> Option<RelayGuard> {
        let registry = self.registry.lock().await;
        if matches!(registry.state, LifecycleState::Draining | LifecycleState::Stopped) {
            return None;
        }
        self.relay_count.fetch_add(1, Ordering::SeqCst);
        Some(RelayGuard { broker: self.clone() })
    }

    /// Count a relay link as being set up, from its accept until the guard drops: when its session
    /// is registered by [`Broker::try_register_relay`], or when it is turned down. Setting a
    /// session up opens the workspace and can take seconds, during which the link is not yet a
    /// relay; without this count a `Stop` arriving then is accepted, and the session it was about
    /// to serve finds its daemon gone. Take the guard before the handshake's first await.
    pub fn begin_relay_handshake(self: &Arc<Self>) -> HandshakeGuard {
        self.relay_handshakes.fetch_add(1, Ordering::SeqCst);
        HandshakeGuard { broker: self.clone() }
    }

    /// Pin an accepted HTTP connection unless shutdown has already started.
    pub async fn try_begin_http_connection(self: &Arc<Self>) -> Option<HttpActivityGuard> {
        let registry = self.registry.lock().await;
        if matches!(registry.state, LifecycleState::Draining | LifecycleState::Stopped) {
            return None;
        }
        Some(self.begin_http_request())
    }

    /// Refuse a user-requested stop while it would disconnect another persistent client.
    pub(super) async fn on_stop(&self) -> CommsResponse {
        {
            let mut registry = self.registry.lock().await;
            let relays = self.relay_count.load(Ordering::SeqCst) + self.relay_handshakes.load(Ordering::SeqCst);
            let http = self.http_inflight.load(Ordering::SeqCst);
            let work = self.work_inflight.load(Ordering::SeqCst);
            if relays + http + work != 0 {
                return CommsResponse::Error {
                    code: "daemon_busy".to_string(),
                    message: format!(
                        "refusing to stop while persistent clients or work are active \
                         (relays={relays}, http={http}, work={work})"
                    ),
                };
            }
            registry.state = LifecycleState::Draining;
        }
        self.finish_drain().await;
        CommsResponse::Ok
    }

    /// Enter the Draining state and fire the accept-loop
    /// shutdown signal so the front-end stops accepting. Firing the signal is what makes a `Stop`
    /// RPC (and SIGTERM/idle-reap/ownership-loss, which all route here) actually terminate the
    /// daemon rather than merely notify connected clients. Idempotent — repeated drains re-send
    /// `true`, which the watch receiver already holds.
    pub async fn begin_drain(&self) {
        self.registry.lock().await.state = LifecycleState::Draining;
        self.finish_drain().await;
    }

    /// The tail shared by [`Broker::begin_drain`] and [`Broker::try_begin_idle_drain`]: cancel
    /// in-flight scans, then fire the accept-loop shutdown signal.
    pub(super) async fn finish_drain(&self) {
        // Trip the scan token FIRST: a mid-flight rescan must start winding down before the ~keep
        // runtime teardown, or the teardown blocks on it. ~keep
        self.scan_cancel.cancel();
        // A waiting `agent_inbox` can wait 40 s, longer than the drain grace: end it now. ~keep
        self.agents.shut_down();
        if let Some(shutdown) = self.shutdown.get() {
            let _ = shutdown.send(true);
        }
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> LifecycleState {
        self.registry.lock().await.state
    }
}
