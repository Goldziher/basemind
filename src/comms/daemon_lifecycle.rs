//! Admission accounting for persistent clients and non-destructive user-requested shutdown.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::daemon::{Broker, LifecycleState, RelayGuard};
use super::daemon_http::HttpActivityGuard;
use super::protocol::{CommsNotification, CommsOut, CommsResponse};

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
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut registry = self.registry.lock().await;
            let relays = self.relay_count.load(Ordering::SeqCst);
            let subscriptions = self.subscriber_count.load(Ordering::SeqCst);
            let http = self.http_inflight.load(Ordering::SeqCst);
            let work = self.work_inflight.load(Ordering::SeqCst);
            let active = relays + subscriptions + http + work;
            if active != 0 {
                return CommsResponse::Error {
                    code: "daemon_busy".to_string(),
                    message: format!(
                        "refusing to stop while persistent clients or work are active \
                         (relays={relays}, subscriptions={subscriptions}, http={http}, work={work})"
                    ),
                };
            }
            registry.state = LifecycleState::Draining;
            registry.sinks.values().map(|sink| sink.tx.clone()).collect()
        };
        self.finish_drain(sinks).await;
        CommsResponse::Ok
    }

    /// Enter the Draining state, notify every live sink to disconnect, and fire the accept-loop
    /// shutdown signal so the front-end stops accepting. Firing the signal is what makes a `Stop`
    /// RPC (and SIGTERM/idle-reap/ownership-loss, which all route here) actually terminate the
    /// daemon rather than merely notify connected clients. Idempotent — repeated drains re-send
    /// `true`, which the watch receiver already holds.
    pub async fn begin_drain(&self) {
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut reg = self.registry.lock().await;
            reg.state = LifecycleState::Draining;
            reg.sinks.values().map(|s| s.tx.clone()).collect()
        };
        self.finish_drain(sinks).await;
    }

    /// The tail shared by [`Broker::begin_drain`] and [`Broker::try_begin_idle_drain`]: tell every
    /// live sink we are going away, then fire the accept-loop shutdown signal. Split out so the
    /// idle path can make its decision under the registry lock without holding it across the sends.
    pub(super) async fn finish_drain(&self, sinks: Vec<mpsc::Sender<CommsOut>>) {
        // Trip the scan token FIRST: a mid-flight rescan must start winding down before (not ~keep
        // after) clients are told to disconnect, or the runtime teardown blocks on it. ~keep
        self.scan_cancel.cancel();
        for tx in sinks {
            let _ = tx.send(CommsOut::Notification(CommsNotification::Shutdown)).await;
        }
        if let Some(shutdown) = self.shutdown.get() {
            let _ = shutdown.send(true);
        }
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> LifecycleState {
        self.registry.lock().await.state
    }
}
