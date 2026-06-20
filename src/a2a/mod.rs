//! A2A (Agent-to-Agent) protocol bindings.
//!
//! Official A2A gRPC service (`lf.a2a.v1.A2AService`, 11 RPCs) + a JSON-RPC 2.0 /
//! agent-card / SSE binding, both served on one axum app ([`server`]) backed by
//! the [`core`] task domain through the shared [`state::A2aState`]. The binary
//! reaches it through the public [`run_server`] entry point (`basemind a2a serve`).

pub(crate) mod core;
pub(crate) mod grpc;
pub(crate) mod jsonrpc;
pub mod proto;
pub(crate) mod server;
pub(crate) mod state;

/// Crate-internal handle on the generated `lf.a2a.v1` package (prost message structs plus the
/// tonic `a2a_service_client` / `a2a_service_server` modules). Kept `pub(crate)` until a later
/// phase needs to expose the full generated surface; external callers use the flat aliases below.
pub(crate) use proto::lf::a2a::v1;

// Flat aliases for the most commonly reached surface.
pub use v1::a2a_service_client::A2aServiceClient;
pub use v1::a2a_service_server::{A2aService, A2aServiceServer};

/// Options for the [`run_server`] entry point (`basemind a2a serve`).
#[derive(Debug, Clone)]
pub struct A2aServeOptions {
    /// Address to bind the combined gRPC + JSON-RPC + SSE listener.
    pub addr: std::net::SocketAddr,
    /// Agent name advertised in the agent card (defaults to "basemind").
    pub name: Option<String>,
    /// Agent description advertised in the agent card.
    pub description: Option<String>,
}

/// Build the A2A server state and serve the combined gRPC + JSON-RPC + SSE app on
/// `opts.addr` until Ctrl-C, then drain gracefully.
///
/// Blocks the calling thread on a fresh multi-thread tokio runtime. This is the
/// public entry point the `basemind a2a serve` CLI dispatches to.
///
/// # Errors
///
/// Returns the bind / runtime-build / serve [`std::io::Error`] if the listener
/// cannot be established or the server loop fails.
pub fn run_server(opts: A2aServeOptions) -> std::io::Result<()> {
    let mut card = state::AgentCardInfo::default();
    if let Some(name) = opts.name {
        card.name = name;
    }
    if let Some(description) = opts.description {
        card.description = description;
    }
    // One listener serves both bindings (axum auto-negotiates HTTP/1.1 + h2c), so
    // the gRPC and JSON-RPC interfaces advertise the same base URL.
    let url = format!("http://{}", opts.addr);
    card.http_url.clone_from(&url);
    card.grpc_url = url;

    let app_state = state::A2aState::new(card);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let cancel = tokio_util::sync::CancellationToken::new();
        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl-C received; shutting down A2A server");
                signal_cancel.cancel();
            }
        });
        server::serve(app_state, opts.addr, cancel).await
    })
}
