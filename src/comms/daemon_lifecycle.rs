//! Admission accounting for persistent clients and non-destructive user-requested shutdown.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::daemon::{Broker, LifecycleState, RelayGuard};
use super::daemon_http::HttpActivityGuard;
use super::protocol::{CommsOut, CommsResponse};

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
}
