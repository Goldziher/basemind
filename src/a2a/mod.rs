//! A2A (Agent-to-Agent) protocol bindings.
//!
//! Official A2A gRPC service (`lf.a2a.v1.A2AService`, 11 RPCs) + a JSON-RPC 2.0 /
//! agent-card / SSE binding, both served on one axum app ([`server`]) backed by
//! the [`core`] task domain through the shared [`state::A2aState`]. The binary
//! reaches it through the public [`run_server`] entry point (`basemind a2a serve`).

pub(crate) mod auth;
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
    /// Explicit bearer token required on every request except the public agent
    /// card. Takes precedence over [`token_file`](Self::token_file).
    pub token: Option<String>,
    /// Path to a bearer-token file, auto-created with `0600` permissions when
    /// missing. Ignored when [`token`](Self::token) is set.
    pub token_file: Option<std::path::PathBuf>,
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

    // Resolve the bearer token: an explicit `--token` wins; otherwise a
    // `--token-file` is read (and auto-generated 0600 when missing); otherwise
    // auth is disabled.
    let auth_token: Option<std::sync::Arc<str>> = if let Some(token) = opts.token.as_deref() {
        let token = token.trim();
        if token.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--token must not be empty",
            ));
        }
        Some(std::sync::Arc::from(token))
    } else if let Some(path) = opts.token_file.as_deref() {
        let token = auth::load_or_create_token(path)?;
        tracing::info!(path = %path.display(), "A2A bearer auth enabled (token file)");
        Some(std::sync::Arc::from(token.as_str()))
    } else {
        None
    };

    // Bind-safety: never expose a non-loopback interface without auth.
    if auth_token.is_none() && !opts.addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to bind non-loopback address {} without auth; pass --token or --token-file",
                opts.addr
            ),
        ));
    }
    if auth_token.is_some() {
        tracing::info!("A2A bearer auth required on all routes except the public agent card");
    }

    let app_state = state::A2aState::new(card).with_auth_token(auth_token);
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
