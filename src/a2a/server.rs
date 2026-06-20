//! A2A HTTP server assembly: the single-listener axum app fronting the
//! JSON-RPC 2.0 binding ([`crate::a2a::jsonrpc::handlers`]) and the tonic gRPC
//! service ([`crate::a2a::grpc::service::BasemindA2aService`]).
//!
//! [`build_router`] wires the routes plus a shared tower middleware stack
//! (request-id, tracing, CORS, load-shed, concurrency limit, timeout). Both
//! transports share one [`tokio::net::TcpListener`]: `axum::serve` auto-negotiates
//! HTTP/1.1 (JSON-RPC) and HTTP/2 h2c (gRPC) per connection, so the gRPC service
//! is mounted as a plain route rather than on a second port.
//!
//! [`serve`] binds the listener and runs the app with graceful shutdown driven
//! by a [`CancellationToken`](tokio_util::sync::CancellationToken). It is mounted
//! by [`crate::a2a::run_server`] (the `basemind a2a serve` CLI). Bearer auth is
//! applied here via [`crate::a2a::auth::require_bearer`].
//!
//! When a TLS pair ([`crate::a2a::TlsPaths`]) is supplied, [`serve`] swaps the
//! plaintext `axum::serve` acceptor for an `axum_server` rustls acceptor whose
//! ALPN list is `["h2", "http/1.1"]`, so gRPC-over-TLS negotiates HTTP/2. The
//! plaintext path is left byte-for-byte unchanged (it depends on axum's HTTP/2
//! h2c upgrade for gRPC). Graceful shutdown for the TLS path is wired through an
//! [`axum_server::Handle`] driven by the same [`CancellationToken`].

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower::limit::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::a2a::auth::require_bearer;
use crate::a2a::jsonrpc::handlers::{agent_card_handler, jsonrpc_handler};
use crate::a2a::state::A2aState;

/// Maximum JSON-RPC request body size (4 MiB). Applied to the JSON-RPC route
/// only so it does not throttle the gRPC streaming path.
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Maximum number of in-flight requests admitted concurrently before the
/// load-shed layer rejects new work with `503 Service Unavailable`.
const MAX_CONCURRENT_REQUESTS: usize = 1024;

/// Per-request timeout, in seconds, enforced by the tower [`TimeoutLayer`].
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Well-known route serving the public agent card. Always reachable without
/// auth so clients can discover the security scheme before holding a token.
pub(crate) const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// gRPC route template for the A2A service. tonic dispatches on the
/// `/<package>.<Service>/<Method>` path; `:method` captures the RPC name.
const GRPC_SERVICE_PATH: &str = "/lf.a2a.v1.A2AService/:method";

/// Build the A2A axum [`Router`]: JSON-RPC entrypoint, agent-card route, the
/// mounted gRPC service, and the shared tower middleware stack.
pub(crate) fn build_router(state: A2aState) -> Router {
    // Mount the tonic service as a plain axum route on the shared listener.
    // `axum::serve` upgrades HTTP/2 h2c per connection, so gRPC clients reach
    // this route over the same port as the HTTP/1.1 JSON-RPC binding. The tonic
    // service body is `tonic::body::BoxBody`; `route_service` unifies it with
    // axum's response body directly, so no body adapter is required here.
    let grpc = crate::a2a::A2aServiceServer::new(
        crate::a2a::grpc::service::BasemindA2aService::new(state.clone()),
    );

    // The fallible middleware (timeout / load-shed / concurrency limit) produces
    // a `BoxError`; axum's final service must be `Infallible`, so `HandleErrorLayer`
    // converts those errors into a `StatusCode` response. It must wrap the layers
    // that can error, so it sits outermost inside the fallible segment.
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        // Bearer auth runs immediately after request-id/trace and BEFORE the
        // concurrency-limit/load-shed/timeout layers, so unauthenticated requests
        // are rejected before they consume a concurrency slot. It covers the
        // JSON-RPC and gRPC routes alike (shared listener) and lets the public
        // agent card through; it is a no-op when auth is disabled.
        .layer(from_fn_with_state(state.clone(), require_bearer))
        .layer(CorsLayer::permissive())
        .layer(HandleErrorLayer::new(handle_middleware_error))
        .layer(LoadShedLayer::new())
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(TimeoutLayer::new(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
        .layer(PropagateRequestIdLayer::x_request_id());

    Router::new()
        .route(
            "/",
            post(jsonrpc_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .route(AGENT_CARD_PATH, get(agent_card_handler))
        .route_service(GRPC_SERVICE_PATH, grpc)
        .layer(middleware)
        .with_state(state)
}

/// Map a tower middleware error onto an HTTP status code. Load-shed rejections
/// become `503`, timeouts `408`, anything else `500`.
async fn handle_middleware_error(err: tower::BoxError) -> StatusCode {
    if err.is::<tower::load_shed::error::Overloaded>() {
        StatusCode::SERVICE_UNAVAILABLE
    } else if err.is::<tower::timeout::error::Elapsed>() {
        StatusCode::REQUEST_TIMEOUT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// Grace period granted to in-flight connections after shutdown is signalled on
/// the TLS path before they are forcibly dropped.
const TLS_SHUTDOWN_GRACE: Duration = Duration::from_secs(REQUEST_TIMEOUT_SECS);

/// Bind `addr` and serve the A2A app until `cancel` fires, then drain gracefully.
///
/// When `tls` is `None` the plaintext path is used verbatim (`axum::serve`,
/// which auto-negotiates HTTP/1.1 + HTTP/2 h2c — gRPC depends on the h2c
/// upgrade). When `tls` is `Some`, [`serve_tls`] terminates TLS via rustls with
/// ALPN `["h2", "http/1.1"]`.
pub(crate) async fn serve(
    state: A2aState,
    addr: SocketAddr,
    cancel: CancellationToken,
    tls: Option<crate::a2a::TlsPaths>,
) -> std::io::Result<()> {
    match tls {
        Some(tls) => serve_tls(state, addr, cancel, tls).await,
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let bound = listener.local_addr()?;
            tracing::info!(address = %bound, tls = false, "A2A HTTP server listening");

            axum::serve(listener, build_router(state))
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
        }
    }
}

/// Serve the A2A app over TLS, terminating with rustls and negotiating HTTP/2
/// vs HTTP/1.1 via ALPN so gRPC-over-TLS works.
///
/// Graceful shutdown is wired through an [`axum_server::Handle`]: a task awaits
/// `cancel` and then calls [`Handle::graceful_shutdown`] with a bounded grace
/// period, mirroring the plaintext path's `with_graceful_shutdown` semantics.
///
/// Never logs key material — only the bound address and the cert/key *paths*.
async fn serve_tls(
    state: A2aState,
    addr: SocketAddr,
    cancel: CancellationToken,
    tls: crate::a2a::TlsPaths,
) -> std::io::Result<()> {
    // Both the aws-lc-rs and ring rustls providers are present in the dependency
    // tree, so the process-default crypto provider is ambiguous and building a
    // `ServerConfig` would panic. Install aws-lc-rs explicitly (idempotent: a
    // prior install by another component is fine, hence the discarded result).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // `RustlsConfig::from_pem_file` builds a `ServerConfig` whose `alpn_protocols`
    // is `["h2", "http/1.1"]`, which is exactly what gRPC-over-TLS needs.
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
        .await
        .map_err(|err| std::io::Error::new(err.kind(), format!("loading TLS cert/key: {err}")))?;

    let handle = axum_server::Handle::new();
    // Bridge the CancellationToken to axum_server's graceful shutdown.
    let shutdown_handle = handle.clone();
    let shutdown_cancel = cancel.clone();
    let shutdown_task = tokio::spawn(async move {
        shutdown_cancel.cancelled().await;
        shutdown_handle.graceful_shutdown(Some(TLS_SHUTDOWN_GRACE));
    });

    tracing::info!(
        address = %addr,
        tls = true,
        cert = %tls.cert.display(),
        key = %tls.key.display(),
        "A2A HTTPS server listening",
    );

    let result = axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(build_router(state).into_make_service())
        .await;

    // If serve returned for a reason OTHER than cancellation (bind/IO error), the
    // bridge task is still parked on `cancelled()`; abort it so it can't linger
    // until runtime shutdown.
    shutdown_task.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    /// Read a response body fully and parse it as JSON.
    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body must read");
        serde_json::from_slice(&bytes).expect("body must be valid JSON")
    }

    /// Build a JSON-RPC POST request against the root route.
    fn jsonrpc_request(payload: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .expect("request must build")
    }

    /// Build a JSON-RPC POST request carrying an `Authorization: Bearer` header.
    fn jsonrpc_request_with_bearer(payload: Value, token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(payload.to_string()))
            .expect("request must build")
    }

    /// A minimal valid `message/send` payload.
    fn message_send_payload() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "",
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hi"}]
                }
            }
        })
    }

    /// State with bearer auth enabled for `token`.
    fn authed_state(token: &str) -> A2aState {
        A2aState::default().with_auth_token(Some(std::sync::Arc::from(token)))
    }

    #[tokio::test]
    async fn agent_card_route_serves_basemind_jsonrpc_card() {
        let app = build_router(A2aState::default());
        let req = Request::builder()
            .method("GET")
            .uri(AGENT_CARD_PATH)
            .body(Body::empty())
            .expect("request must build");

        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["name"], json!("basemind"));
        assert_eq!(body["preferredTransport"], json!("JSONRPC"));
    }

    #[tokio::test]
    async fn extended_card_method_returns_basemind_result() {
        let app = build_router(A2aState::default());
        let req = jsonrpc_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent/getAuthenticatedExtendedCard",
            "params": {}
        }));

        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["result"]["name"], json!("basemind"));
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let app = build_router(A2aState::default());
        let req = jsonrpc_request(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "does/not-exist",
            "params": {}
        }));

        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], json!(-32601));
    }

    #[tokio::test]
    async fn message_send_returns_task_result() {
        let app = build_router(A2aState::default());
        let req = jsonrpc_request(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "",
                    "role": "user",
                    "parts": [{"kind": "text", "text": "do something"}]
                }
            }
        }));

        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["result"]["kind"], json!("task"));
    }

    #[tokio::test]
    async fn auth_rejects_request_without_token() {
        let app = build_router(authed_state("secret-token"));
        let resp = app
            .oneshot(jsonrpc_request(message_send_payload()))
            .await
            .expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer"),
        );
    }

    #[tokio::test]
    async fn auth_rejects_request_with_wrong_token() {
        let app = build_router(authed_state("secret-token"));
        let resp = app
            .oneshot(jsonrpc_request_with_bearer(message_send_payload(), "nope"))
            .await
            .expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_allows_request_with_correct_token() {
        let app = build_router(authed_state("secret-token"));
        let resp = app
            .oneshot(jsonrpc_request_with_bearer(
                message_send_payload(),
                "secret-token",
            ))
            .await
            .expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["result"]["kind"], json!("task"));
    }

    #[tokio::test]
    async fn agent_card_is_public_even_when_auth_enabled() {
        let app = build_router(authed_state("secret-token"));
        let req = Request::builder()
            .method("GET")
            .uri(AGENT_CARD_PATH)
            .body(Body::empty())
            .expect("request must build");
        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        // Auth-on card advertises the bearer security scheme.
        assert_eq!(body["securitySchemes"]["bearer"]["scheme"], json!("bearer"));
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let app = build_router(A2aState::default());
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("{ not json"))
            .expect("request must build");
        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], json!(-32700));
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_returns_invalid_request() {
        let app = build_router(A2aState::default());
        let req = jsonrpc_request(json!({
            "jsonrpc": "1.0",
            "id": 7,
            "method": "message/send",
            "params": {}
        }));
        let resp = app.oneshot(req).await.expect("oneshot must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], json!(-32600));
        assert_eq!(body["id"], json!(7));
    }

    // --- TLS path ---------------------------------------------------------

    /// Test-only certificate verifier that trusts any server certificate.
    ///
    /// This lives in `#[cfg(test)]` and is used ONLY by the in-test TLS client so
    /// it can connect to the self-signed server. Production code never weakens
    /// verification; the server side performs no client-cert checks either way.
    #[derive(Debug)]
    struct TrustAnyServerCert;

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for TrustAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
            _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: tokio_rustls::rustls::pki_types::UnixTime,
        ) -> Result<
            tokio_rustls::rustls::client::danger::ServerCertVerified,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// End-to-end TLS handshake: start `serve_tls` with a self-signed cert,
    /// connect over real TLS with a permissive client, and assert the public
    /// agent card returns `200 OK` over HTTPS. Exercises the full B4.3 path
    /// (cert load + ALPN + bind) that the validation unit tests cannot.
    #[tokio::test]
    async fn serve_tls_serves_agent_card_over_https() {
        use std::io::Write as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // Self-signed cert/key via rcgen (no committed secrets).
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate self-signed cert");
        let mut cert_file = tempfile::NamedTempFile::new().expect("cert temp file");
        cert_file
            .write_all(cert.cert.pem().as_bytes())
            .expect("write cert pem");
        cert_file.flush().expect("flush cert");
        let mut key_file = tempfile::NamedTempFile::new().expect("key temp file");
        key_file
            .write_all(cert.key_pair.serialize_pem().as_bytes())
            .expect("write key pem");
        key_file.flush().expect("flush key");

        let tls = crate::a2a::resolve_tls_config(Some(cert_file.path()), Some(key_file.path()))
            .expect("config must validate")
            .expect("both supplied yields Some");

        // Bind an ephemeral loopback port; reuse it for the server.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("probe bind");
        let addr = probe.local_addr().expect("probe addr");
        drop(probe);

        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server =
            tokio::spawn(
                async move { serve_tls(A2aState::default(), addr, server_cancel, tls).await },
            );

        // Build a permissive TLS client config with HTTP/1.1 ALPN so the request
        // is a plain HTTP/1.1 GET.
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut client_config = tokio_rustls::rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(TrustAnyServerCert))
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));
        let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
            .expect("server name");

        // Retry the connect until the server has bound (graceful startup race).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let tcp = loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(stream) => break stream,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => panic!("server never accepted TLS connections: {err}"),
            }
        };
        let mut tls_stream = connector
            .connect(server_name, tcp)
            .await
            .expect("TLS handshake must succeed");

        let request = format!(
            "GET {AGENT_CARD_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        tls_stream
            .write_all(request.as_bytes())
            .await
            .expect("write request over TLS");
        tls_stream.flush().await.expect("flush TLS request");

        let mut raw = Vec::new();
        tls_stream
            .read_to_end(&mut raw)
            .await
            .expect("read TLS response");
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "agent card must return 200 over TLS, got head: {head}"
        );
        assert!(
            body.contains("\"basemind\""),
            "agent card body must name basemind over TLS: {body}"
        );

        cancel.cancel();
        let _ = server.await;
    }
}
