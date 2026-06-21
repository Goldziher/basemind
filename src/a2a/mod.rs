//! A2A (Agent-to-Agent) protocol bindings.
//!
//! Official A2A gRPC service (`lf.a2a.v1.A2AService`, 11 RPCs) + a JSON-RPC 2.0 /
//! agent-card / SSE binding, both served on one axum app (`server`) backed by
//! the `core` task domain through the shared `state::A2aState`. The binary
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
    /// PEM certificate (chain) for TLS termination. Must be supplied together
    /// with [`tls_key`](Self::tls_key); supplying exactly one is a usage error.
    /// When both are set the server serves HTTPS and negotiates HTTP/2 vs
    /// HTTP/1.1 via ALPN so gRPC-over-TLS works.
    pub tls_cert: Option<std::path::PathBuf>,
    /// PEM private key matching [`tls_cert`](Self::tls_cert). See that field for
    /// the both-or-neither requirement.
    pub tls_key: Option<std::path::PathBuf>,
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
    // Resolve and validate the optional TLS pair up front: both cert and key must
    // be supplied together (exactly-one is a usage error) and both must be
    // readable before we bind. Returns `None` for the plaintext path.
    let tls = resolve_tls_config(opts.tls_cert.as_deref(), opts.tls_key.as_deref())?;

    // One listener serves both bindings. Over plaintext axum auto-negotiates
    // HTTP/1.1 + h2c; over TLS the ALPN list (`h2`, `http/1.1`) drives the same
    // split. The card URL scheme must reflect whether TLS is on.
    let scheme = if tls.is_some() { "https" } else { "http" };
    let url = format!("{scheme}://{}", opts.addr);
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
        server::serve(app_state, opts.addr, cancel, tls).await
    })
}

/// A validated, readable TLS cert/key path pair for the server's TLS path.
///
/// Constructed only by [`resolve_tls_config`], which enforces the
/// both-or-neither and readability invariants before a value of this type
/// exists. Holds paths only — never key material — so it is safe to log a
/// non-secret summary built from it.
#[derive(Debug, Clone)]
pub(crate) struct TlsPaths {
    /// PEM certificate (chain) path.
    pub(crate) cert: std::path::PathBuf,
    /// PEM private-key path.
    pub(crate) key: std::path::PathBuf,
}

/// Validate the optional `--tls-cert` / `--tls-key` pair.
///
/// Returns `Ok(None)` when neither is supplied (the plaintext path),
/// `Ok(Some(_))` when both are supplied and both files are readable, and an
/// [`std::io::Error`] when exactly one is supplied (usage error) or a supplied
/// file cannot be read.
///
/// # Errors
///
/// - [`InvalidInput`](std::io::ErrorKind::InvalidInput) when exactly one of
///   cert/key is supplied.
/// - The underlying read error (e.g. [`NotFound`](std::io::ErrorKind::NotFound)
///   or a permission error) when a supplied path is not readable, wrapped with
///   context naming the offending path.
pub(crate) fn resolve_tls_config(
    cert: Option<&std::path::Path>,
    key: Option<&std::path::Path>,
) -> std::io::Result<Option<TlsPaths>> {
    match (cert, key) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--tls-cert and --tls-key must be supplied together (both or neither)",
        )),
        (Some(cert), Some(key)) => {
            // Probe readability before bind so a missing/unreadable file fails
            // fast with a path-tagged error instead of mid-handshake. We read the
            // bytes only to confirm access and immediately drop them; the actual
            // parse happens in the server via `RustlsConfig::from_pem_file`.
            ensure_readable(cert)?;
            ensure_readable(key)?;
            ensure_key_not_group_readable(key)?;
            Ok(Some(TlsPaths {
                cert: cert.to_path_buf(),
                key: key.to_path_buf(),
            }))
        }
    }
}

/// Confirm `path` is readable, mapping any failure to a path-tagged
/// [`std::io::Error`]. Reads and drops the bytes — never logs or returns them.
fn ensure_readable(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)
        .map(|_| ())
        .map_err(|err| std::io::Error::new(err.kind(), format!("{}: {err}", path.display())))
}

/// Reject a TLS private key file readable by group or other. A leaked key
/// undermines the TLS it backs, so we fail closed — the same discipline applied
/// to the bearer token file in [`auth::load_or_create_token`]. No-op off Unix.
#[cfg(unix)]
fn ensure_key_not_group_readable(key: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(key)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "TLS key '{}' is group/other accessible (mode {mode:o}); expected 0600",
                key.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_key_not_group_readable(_key: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_tls_config;
    use std::io::Write as _;
    use std::path::Path;

    /// Write `bytes` to a fresh temp file and return its handle (kept alive so the
    /// path stays valid for the duration of the test).
    fn temp_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        file.write_all(bytes).expect("write temp file");
        file.flush().expect("flush temp file");
        file
    }

    #[test]
    fn neither_cert_nor_key_yields_plaintext() {
        let resolved = resolve_tls_config(None, None).expect("neither is valid");
        assert!(
            resolved.is_none(),
            "no TLS pair must map to the plaintext path"
        );
    }

    #[test]
    fn both_readable_paths_yield_a_config() {
        let cert = temp_file(b"-----BEGIN CERTIFICATE-----\n");
        let key = temp_file(b"-----BEGIN PRIVATE KEY-----\n");
        let resolved = resolve_tls_config(Some(cert.path()), Some(key.path()))
            .expect("both readable must validate");
        let paths = resolved.expect("both supplied must yield Some");
        assert_eq!(paths.cert, cert.path());
        assert_eq!(paths.key, key.path());
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_key_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let cert = temp_file(b"-----BEGIN CERTIFICATE-----\n");
        let key = temp_file(b"-----BEGIN PRIVATE KEY-----\n");
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        let err = resolve_tls_config(Some(cert.path()), Some(key.path()))
            .expect_err("group/other-readable key must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn cert_without_key_is_rejected() {
        let cert = temp_file(b"x");
        let err = resolve_tls_config(Some(cert.path()), None)
            .expect_err("exactly-one (cert only) must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn key_without_cert_is_rejected() {
        let key = temp_file(b"x");
        let err = resolve_tls_config(None, Some(key.path()))
            .expect_err("exactly-one (key only) must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn missing_cert_file_is_rejected() {
        let key = temp_file(b"x");
        let missing = Path::new("/nonexistent/basemind-a2a-tls/cert.pem");
        let err = resolve_tls_config(Some(missing), Some(key.path()))
            .expect_err("unreadable cert must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("cert.pem"),
            "error must name the offending path: {err}"
        );
    }

    #[test]
    fn missing_key_file_is_rejected() {
        let cert = temp_file(b"x");
        let missing = Path::new("/nonexistent/basemind-a2a-tls/key.pem");
        let err = resolve_tls_config(Some(cert.path()), Some(missing))
            .expect_err("unreadable key must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
