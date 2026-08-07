//! The daemon's second MCP front-end: a stateless streamable-HTTP transport (rmcp 3.0, SEP-2567).
//!
//! The comms daemon already hosts the rmcp code-map router per-connection over its Unix-socket relay
//! (see [`Broker::serve_relay_connection`](super::daemon::Broker::serve_relay_connection)). This adds
//! a SECOND front-end to the SAME daemon: a loopback-TCP HTTP listener that routes each request to
//! the right workspace's shared read stack and serves it statelessly. One daemon, sole fjall writer,
//! now also the MCP HTTP server.
//!
//! ## URL scheme
//!
//! `http://127.0.0.1:<port>/mcp?root=<percent-encoded-abs-repo-path>&agent=<agent-id>`
//!
//! Each POST is self-describing: the router parses `root` + `agent` from the query, resolves the
//! workspace's [`SharedReadStack`](crate::mcp::SharedReadStack) (building it on first touch, shared
//! by `Arc` with any relay client on the same workspace), and constructs a fresh
//! [`StreamableHttpService`] bound to that stack for the one request. There is no session registry
//! to maintain — `NeverSessionManager` keeps the transport stateless per SEP-2567.
//!
//! ## Bind-as-lock + portfile
//!
//! The listener binds a fixed loopback address (default `127.0.0.1:51786`, override
//! [`HTTP_ADDR_ENV`]). Binding IS the lock: only one daemon holds it. The actually-bound `host:port`
//! is written to `<comms_dir>/http.addr` (mode 0600 on Unix) so tooling — `basemind daemon ensure`
//! and any launcher — can discover it without guessing the port.
//!
//! ## Idle lifecycle
//!
//! Stateless HTTP holds no persistent connection, so the daemon's UDS-link-based idle reaper would
//! see it as idle between requests and self-terminate mid-use. Every request is wrapped in a
//! [`Broker::begin_http_request`](super::daemon::Broker::begin_http_request) guard that both pins the
//! daemon while the request is in flight and stamps HTTP recency — see
//! [`Broker::is_idle_for`](super::daemon::Broker::is_idle_for).

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::Service;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::daemon::Broker;
use super::identity;
use super::ids::AgentId;
use crate::mcp::BasemindServer;

/// Env var overriding the bound loopback address. `host:port`, e.g. `127.0.0.1:0` to let the OS pick
/// a free port (the actual port is discoverable via the portfile). Empty/unset uses
/// [`DEFAULT_HTTP_ADDR`].
pub const HTTP_ADDR_ENV: &str = "BASEMIND_HTTP_ADDR";
/// Default fixed loopback address for the streamable-HTTP MCP transport.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:51786";
/// Portfile name under `comms_dir` holding the actually-bound `host:port`, so tooling reads the
/// address instead of assuming the default port.
const PORTFILE_NAME: &str = "http.addr";
/// The MCP JSON-RPC transport path. Anything other than this or [`UI_PATH`] is a 404.
const MCP_PATH: &str = "/mcp";
/// The interactive-UI path (ADR-0006): `GET /ui?root=<abs>&…` serves the self-contained graph page
/// for a workspace, so a browser (or the Tauri shell) can drive the same view the `ui` tool resolves.
const UI_PATH: &str = "/ui";
/// How long [`served_ui_url`] waits for the daemon port to answer before deciding it is not serving.
/// A live loopback daemon answers in well under this; a stale portfile (crashed daemon) fails fast so
/// the `ui` tool degrades to the file export instead of handing back a dead link.
const UI_PROBE_TIMEOUT: Duration = Duration::from_millis(150);
/// Owner-only mode for the portfile, matching the socket's `0600`.
#[cfg(unix)]
const PORTFILE_MODE: u32 = 0o600;
/// Poll cadence while [`await_http_ready`] waits for the listener to answer.
const HTTP_READY_POLL: Duration = Duration::from_millis(50);

/// Response body type produced by every path here — the shape [`StreamableHttpService::handle`]
/// returns, so the router and the service agree.
type HttpBody = BoxBody<Bytes, Infallible>;

/// The portfile path for a resolved comms dir.
pub fn portfile_path(comms_dir: &Path) -> PathBuf {
    comms_dir.join(PORTFILE_NAME)
}

/// Resolve the address to bind: [`HTTP_ADDR_ENV`] when set and non-blank, else [`DEFAULT_HTTP_ADDR`].
fn resolve_addr() -> Result<SocketAddr> {
    let raw = std::env::var(HTTP_ADDR_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_string());
    raw.parse::<SocketAddr>()
        .with_context(|| format!("parse {HTTP_ADDR_ENV}={raw:?} as a host:port socket address"))
}

/// Write the bound `host:port` to the portfile (mode 0600 on Unix).
fn write_portfile(comms_dir: &Path, addr: &SocketAddr) -> std::io::Result<()> {
    let path = portfile_path(comms_dir);
    std::fs::write(&path, addr.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(PORTFILE_MODE))?;
    }
    Ok(())
}

/// Read the bound address back from the portfile, if present and parseable.
fn read_portfile(comms_dir: &Path) -> Option<SocketAddr> {
    std::fs::read_to_string(portfile_path(comms_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Parse `root` (required) and `agent` (optional) from the request query. A blank `agent` is treated
/// as absent so the router falls back to the workspace's stable default identity.
fn parse_target(query: Option<&str>) -> Option<(PathBuf, Option<String>)> {
    let query = query?;
    let mut root: Option<String> = None;
    let mut agent: Option<String> = None;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "root" => root = Some(value.into_owned()),
            "agent" => agent = Some(value.into_owned()),
            _ => {}
        }
    }
    let root = root.filter(|value| !value.trim().is_empty())?;
    let agent = agent.filter(|value| !value.trim().is_empty());
    Some((PathBuf::from(root), agent))
}

/// Build a plain-text HTTP response with a boxed body matching [`HttpBody`].
fn text_response(status: StatusCode, message: &str) -> Response<HttpBody> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        // ~keep A response built from constant parts is infallible; mirrors rmcp's own handlers.
        .body(Full::new(Bytes::from(message.to_string())).boxed())
        .expect("text response builds from constant parts")
}

/// Build an HTML/SVG HTTP response with a boxed body matching [`HttpBody`]. `content_type` is one of
/// the two constants the UI renderer returns, so the header is always valid.
fn html_response(status: StatusCode, body: String, content_type: &str) -> Response<HttpBody> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::from(body)).boxed())
        .expect("html response builds from a valid header + body")
}

/// The `/ui` route's parsed query: the required workspace `root` plus the same graph-shaping knobs the
/// `ui` tool exposes, each defaulted exactly as the tool defaults them so the served page matches.
struct UiRenderArgs {
    root: PathBuf,
    format: String,
    edges: String,
    algorithm: String,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<String>,
}

/// Parse `root` (required) and the optional graph knobs from a `/ui` request query. Returns `None`
/// only when `root` is missing/blank; unknown keys and unparseable numbers are ignored (best-effort,
/// like the tool's own defaulting).
fn parse_ui_args(query: Option<&str>) -> Option<UiRenderArgs> {
    let query = query?;
    let (mut root, mut format, mut edges, mut algorithm, mut focus) = (None, None, None, None, None);
    let (mut min_confidence, mut max_nodes, mut max_edges) = (None, None, None);
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "root" => root = Some(value.into_owned()),
            "format" => format = Some(value.into_owned()),
            "edges" => edges = Some(value.into_owned()),
            "algorithm" | "algo" => algorithm = Some(value.into_owned()),
            "min_confidence" => min_confidence = value.parse::<f32>().ok(),
            "max_nodes" => max_nodes = value.parse::<u32>().ok(),
            "max_edges" => max_edges = value.parse::<u32>().ok(),
            "focus" => focus = Some(value.into_owned()),
            _ => {}
        }
    }
    let non_blank = |value: String| Some(value).filter(|v| !v.trim().is_empty());
    let root = root.and_then(non_blank)?;
    Some(UiRenderArgs {
        root: PathBuf::from(root),
        format: format.and_then(non_blank).unwrap_or_else(|| "html".to_string()),
        edges: edges.and_then(non_blank).unwrap_or_else(|| "all".to_string()),
        algorithm: algorithm
            .and_then(non_blank)
            .unwrap_or_else(|| "label_propagation".to_string()),
        min_confidence,
        max_nodes,
        max_edges,
        focus: focus.and_then(non_blank),
    })
}

/// The per-request router: shared across every accepted connection, cheap to clone.
struct HttpRouter {
    broker: Arc<Broker>,
    /// One stateless session manager shared by every request; it rejects all session ops so the
    /// transport is stateless per SEP-2567.
    session_manager: Arc<NeverSessionManager>,
    /// Parent cancellation token; each request's [`StreamableHttpServerConfig`] gets a child, so a
    /// daemon drain cancels in-flight handlers.
    cancel: CancellationToken,
    /// Whether the bound listener is a loopback address. When true, [`HttpRouter::handle`] enforces
    /// the DNS-rebinding `Host` guard; an explicit non-loopback bind (the documented remote-access
    /// opt-in in [`serve_http`]) disables it, leaving host/origin policy to the operator.
    loopback: bool,
}

/// The host part of a `Host` / `:authority` value, dropping any `:port`. A bracketed IPv6 literal
/// (`[::1]` / `[::1]:port`) keeps its brackets so the caller sees `[::1]`.
fn authority_host(authority: &str) -> &str {
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        };
    }
    match authority.rsplit_once(':') {
        Some((host, _port)) => host,
        None => authority,
    }
}

/// Whether the request's `Host` (HTTP/1.1) or `:authority` (HTTP/2) names a loopback address —
/// `localhost`, `127.0.0.0/8`, or `::1`. A loopback-bound listener accepts only these; any other
/// value is a DNS-rebinding attempt (a page on `evil.example` whose DNS later resolves to
/// `127.0.0.1`, making the loopback server same-origin) and is rejected before the request can name
/// or read a workspace. A missing header is rejected too — legitimate local clients always send one.
fn host_is_loopback(request: &Request<Incoming>) -> bool {
    let raw = match request
        .headers()
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        Some(header) => Some(header),
        None => request.uri().authority().map(|authority| authority.as_str()),
    };
    let Some(raw) = raw else {
        return false;
    };
    let host = authority_host(raw);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

impl HttpRouter {
    /// Route and serve one HTTP request. Always returns a response (errors are HTTP status codes,
    /// never a torn connection), mirroring the relay path's never-brick-a-client contract.
    async fn handle(&self, request: Request<Incoming>) -> Response<HttpBody> {
        // DNS-rebinding guard: a loopback-bound listener answers only requests whose `Host`/
        // `:authority` is a loopback name. This runs first, before the daemon is pinned or any
        // workspace is read, so a foreign-`Host` page (the rebinding-to-127.0.0.1 attack) cannot even
        // stamp HTTP recency. Skipped on an explicit non-loopback bind (the remote-access opt-in in
        // `serve_http`), where the operator owns host/origin policy.
        if self.loopback && !host_is_loopback(&request) {
            return text_response(
                StatusCode::FORBIDDEN,
                "forbidden: Host header is not a loopback address (DNS-rebinding protection)",
            );
        }

        // Pin the daemon (and stamp HTTP recency) for the whole request so the idle reaper cannot
        // tear the process down mid-request. See `Broker::begin_http_request`.
        let _activity = self.broker.begin_http_request();

        // The interactive-UI route (ADR-0006) serves an HTML/SVG page for a browser; it precedes the
        // MCP JSON-RPC branch and is the only other path this server answers.
        if request.uri().path() == UI_PATH {
            return self.handle_ui(&request).await;
        }

        if request.uri().path() != MCP_PATH {
            return text_response(
                StatusCode::NOT_FOUND,
                "not found: this server serves POST /mcp and GET /ui only",
            );
        }
        let Some((raw_root, agent)) = parse_target(request.uri().query()) else {
            return text_response(StatusCode::NOT_FOUND, "not found: missing ?root=<abs-repo-path>");
        };
        let Ok(root) = std::fs::canonicalize(&raw_root) else {
            return text_response(
                StatusCode::NOT_FOUND,
                "not found: root does not resolve to an existing path",
            );
        };

        let shared = match self.broker.host_read_stack(&root).await {
            Ok(shared) => shared,
            Err(error) => {
                tracing::warn!(%error, root = %root.display(), "http: hosting read stack failed");
                return text_response(StatusCode::NOT_FOUND, "not found: workspace could not be hosted");
            }
        };
        // Keep the workspace pinned against eviction for the request's lifetime.
        let _conn = match self.broker.begin_workspace_conn(&root) {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, root = %root.display(), "http: workspace connection accounting failed");
                return text_response(StatusCode::NOT_FOUND, "not found: workspace could not be hosted");
            }
        };

        // Enforce the same `AgentId` invariant the UDS relay does (non-empty, <=128 bytes, charset ~keep
        // `[A-Za-z0-9._:-]`): the agent id becomes the memory-owner / comms-sender key, so a present ~keep
        // but malformed value must be rejected (400) rather than poisoning scoped keys. A missing ~keep
        // agent falls back to the workspace's stable default identity. ~keep
        let agent_id = match agent {
            Some(raw) => match AgentId::parse(raw) {
                Ok(id) => id.into_string(),
                Err(error) => {
                    return text_response(
                        StatusCode::BAD_REQUEST,
                        &format!("bad request: invalid ?agent= ({error})"),
                    );
                }
            },
            None => identity::cli_agent_id(&root).into_string(),
        };
        tracing::debug!(agent = %agent_id, root = %root.display(), "http: serving stateless mcp request");

        // The factory is called per request by rmcp; it has no access to the HTTP request, so the
        // root+agent binding is captured here and a fresh server is minted over the shared stack.
        let factory = move || Ok(BasemindServer::from_shared(shared.clone(), agent_id.clone()));
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(self.cancel.child_token());
        let service = StreamableHttpService::new(factory, self.session_manager.clone(), config);
        service.handle(request).await
    }

    /// Serve `GET /ui?root=<abs>&…`: render the interactive graph page for a workspace and return it
    /// as HTML/SVG. Mirrors [`handle`]'s workspace resolution (canonicalize → host the read stack →
    /// pin the workspace against eviction), then renders through the shared `render_ui_http` path the
    /// `ui` tool also uses. Never bricks the connection — every failure is an HTTP status.
    async fn handle_ui(&self, request: &Request<Incoming>) -> Response<HttpBody> {
        let Some(args) = parse_ui_args(request.uri().query()) else {
            return text_response(StatusCode::BAD_REQUEST, "bad request: missing ?root=<abs-repo-path>");
        };
        let Ok(root) = std::fs::canonicalize(&args.root) else {
            return text_response(
                StatusCode::NOT_FOUND,
                "not found: root does not resolve to an existing path",
            );
        };
        let shared = match self.broker.host_read_stack(&root).await {
            Ok(shared) => shared,
            Err(error) => {
                tracing::warn!(%error, root = %root.display(), "http /ui: hosting read stack failed");
                return text_response(StatusCode::NOT_FOUND, "not found: workspace could not be hosted");
            }
        };
        // Pin the workspace against eviction for the render's lifetime, mirroring the MCP branch.
        let _conn = match self.broker.begin_workspace_conn(&root) {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, root = %root.display(), "http /ui: workspace connection accounting failed");
                return text_response(StatusCode::NOT_FOUND, "not found: workspace could not be hosted");
            }
        };
        let agent_id = identity::cli_agent_id(&root).into_string();
        let server = BasemindServer::from_shared(shared, agent_id);
        match server
            .render_ui_http(
                &args.format,
                &args.edges,
                &args.algorithm,
                args.min_confidence,
                args.max_nodes,
                args.max_edges,
                args.focus,
            )
            .await
        {
            Ok((body, content_type)) => html_response(StatusCode::OK, body, content_type),
            Err(error) => text_response(StatusCode::BAD_REQUEST, &format!("bad request: {}", error.message)),
        }
    }
}

/// A [`hyper::service::Service`] wrapper over the shared router. A newtype is required because the
/// orphan rules forbid implementing the foreign `Service` trait directly for `Arc<HttpRouter>`.
#[derive(Clone)]
struct HyperSvc(Arc<HttpRouter>);

impl Service<Request<Incoming>> for HyperSvc {
    type Response = Response<HttpBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        let router = self.0.clone();
        Box::pin(async move { Ok(router.handle(request).await) })
    }
}

/// Serve the streamable-HTTP MCP transport until `shutdown` flips true (or its sender drops). Binds
/// the loopback listener (bind-as-lock), writes the portfile, and serves each connection with a
/// hyper auto (h1/h2) server over the shared router. A bind failure is returned to the caller, which
/// logs it and leaves the rest of the daemon (the UDS relay) running — HTTP is additive, never a
/// precondition for comms.
pub async fn serve_http(broker: Arc<Broker>, comms_dir: PathBuf, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let addr = resolve_addr()?;
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "BASEMIND_HTTP_ADDR binds a NON-loopback address: the MCP transport becomes reachable \
             off-host. rmcp Host-header (DNS-rebinding) validation still applies, but for genuine \
             remote access you must configure allowed_hosts/origins for the real hostnames."
        );
    }
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            // A stale portfile from a prior crashed daemon would otherwise misdirect
            // `await_http_ready` / launchers / hard-coded manifests to whatever now holds the port.
            // Clear it before bailing so discovery fails cleanly instead of pointing at a foreign
            // process. (The portfile lives in this user's comms dir, so it can only be our own stale
            // write — the per-user singleton lock means we never race another basemind daemon here.)
            let path = portfile_path(&comms_dir);
            if let Err(remove_error) = std::fs::remove_file(&path)
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(error = %remove_error, path = %path.display(),
                    "http: clearing stale portfile after bind failure");
            }
            return Err(anyhow::Error::new(error)).with_context(|| {
                format!("bind streamable-HTTP MCP listener on {addr} (is another process holding it?)")
            });
        }
    };
    let local = listener.local_addr().context("read the bound HTTP address")?;
    write_portfile(&comms_dir, &local).with_context(|| format!("write HTTP portfile under {}", comms_dir.display()))?;
    tracing::info!(addr = %local, "comms: streamable-HTTP MCP transport listening");

    let cancel = CancellationToken::new();
    let router = Arc::new(HttpRouter {
        broker,
        session_manager: Arc::new(NeverSessionManager::default()),
        cancel: cancel.clone(),
        loopback: local.ip().is_loopback(),
    });

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // A drain sends `true`; a dropped sender ends the daemon too. Either stops accepts.
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(%error, "http: accept failed");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let service = HyperSvc(router.clone());
                let conn_cancel = cancel.clone();
                // Pin the daemon for the connection's whole lifetime, incremented HERE — before the
                // task is spawned — so there is no accept→handler gap the idle reaper can slip
                // through and tear down the process on the first request after idle (the exact
                // wake-from-idle case the pin exists to protect). `handle` also pins per request to
                // refresh HTTP recency; nested counting on the atomic is fine.
                let Some(conn_activity) = router.broker.try_begin_http_connection().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let _conn_activity = conn_activity;
                    let builder = auto::Builder::new(TokioExecutor::new());
                    tokio::select! {
                        result = builder.serve_connection(io, service) => {
                            if let Err(error) = result {
                                tracing::debug!(error = %error, "http: connection ended with error");
                            }
                        }
                        _ = conn_cancel.cancelled() => {}
                    }
                });
            }
        }
    }

    cancel.cancel();
    let path = portfile_path(&comms_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!(%error, path = %path.display(), "http: removing portfile failed"),
    }
    tracing::info!("comms: streamable-HTTP MCP transport stopped");
    Ok(())
}

/// Poll until the transport answers a TCP connect (or `timeout` elapses), returning the bound
/// `host:port`. Used by `basemind daemon ensure` to confirm readiness before printing the base URL.
pub async fn await_http_ready(comms_dir: &Path, timeout: Duration) -> Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(addr) = read_portfile(comms_dir)
            && tokio::net::TcpStream::connect(addr).await.is_ok()
        {
            return Ok(addr.to_string());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("streamable-HTTP MCP transport did not become ready within {timeout:?}");
        }
        tokio::time::sleep(HTTP_READY_POLL).await;
    }
}

/// The MCP base URL for a bound `host:port`.
pub fn base_url(addr: &str) -> String {
    format!("http://{addr}{MCP_PATH}")
}

/// Assemble the `/ui` URL for a served workspace: `http://<addr>/ui?root=…&…` with every value
/// percent-encoded. Pure (no I/O) so the URL assembly is unit-testable.
#[allow(clippy::too_many_arguments)]
fn build_ui_url(
    addr: &SocketAddr,
    root: &Path,
    format: &str,
    edges: &str,
    algorithm: &str,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<&str>,
) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    ser.append_pair("root", &root.to_string_lossy());
    ser.append_pair("format", format);
    ser.append_pair("edges", edges);
    ser.append_pair("algorithm", algorithm);
    if let Some(confidence) = min_confidence {
        ser.append_pair("min_confidence", &confidence.to_string());
    }
    if let Some(max) = max_nodes {
        ser.append_pair("max_nodes", &max.to_string());
    }
    if let Some(max) = max_edges {
        ser.append_pair("max_edges", &max.to_string());
    }
    if let Some(prefix) = focus {
        ser.append_pair("focus", prefix);
    }
    format!("http://{addr}/ui?{}", ser.finish())
}

/// Resolve the live `/ui` URL for `root` when a basemind daemon is serving HTTP on this machine, else
/// `None` (portfile absent/unreadable, or the port is not answering). Called by the `ui` tool
/// (`crate::mcp`) to prefer a live served page over the static file export.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn served_ui_url(
    root: &Path,
    format: &str,
    edges: &str,
    algorithm: &str,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<&str>,
) -> Option<String> {
    let paths = super::singleton::resolve_paths().ok()?;
    served_ui_url_from_comms_dir(
        &paths.comms_dir,
        root,
        format,
        edges,
        algorithm,
        min_confidence,
        max_nodes,
        max_edges,
        focus,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn served_ui_url_from_comms_dir(
    comms_dir: &Path,
    root: &Path,
    format: &str,
    edges: &str,
    algorithm: &str,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<&str>,
) -> Option<String> {
    let addr = read_portfile(comms_dir)?;
    // Confirm a daemon is actually answering before advertising the URL — a stale portfile (crashed
    // daemon) must degrade to the file export, not hand back a dead link. Async connect + timeout so
    // the probe never blocks the calling tokio worker (`run_ui` awaits this on a hot path).
    tokio::time::timeout(UI_PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    Some(build_ui_url(
        &addr,
        root,
        format,
        edges,
        algorithm,
        min_confidence,
        max_nodes,
        max_edges,
        focus,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_reads_root_and_agent() {
        let (root, agent) = parse_target(Some("root=%2Ftmp%2Fmy%20repo&agent=alice")).expect("parses");
        assert_eq!(root, PathBuf::from("/tmp/my repo"));
        assert_eq!(agent.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_target_treats_blank_agent_as_absent() {
        let (root, agent) = parse_target(Some("root=%2Ftmp%2Fr&agent=%20%20")).expect("parses");
        assert_eq!(root, PathBuf::from("/tmp/r"));
        assert_eq!(agent, None);
    }

    #[test]
    fn parse_target_requires_root() {
        assert!(parse_target(Some("agent=alice")).is_none());
        assert!(parse_target(Some("root=")).is_none());
        assert!(parse_target(None).is_none());
    }

    #[test]
    fn resolve_addr_defaults_and_honors_env() {
        // Default when unset.
        // SAFETY: single-threaded test; no other thread reads the env concurrently.
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        assert_eq!(resolve_addr().expect("default parses").to_string(), DEFAULT_HTTP_ADDR);
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "127.0.0.1:0") };
        assert_eq!(
            resolve_addr().expect("env parses"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap()
        );
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
    }

    #[test]
    fn base_url_appends_mcp_path() {
        assert_eq!(base_url("127.0.0.1:51786"), "http://127.0.0.1:51786/mcp");
    }

    #[test]
    fn build_ui_url_encodes_root_and_all_knobs() {
        let addr: SocketAddr = "127.0.0.1:51786".parse().unwrap();
        let url = build_ui_url(
            &addr,
            Path::new("/tmp/my repo"),
            "html",
            "all",
            "label_propagation",
            Some(0.5),
            Some(200),
            Some(700),
            Some("src/mcp"),
        );
        assert!(url.starts_with("http://127.0.0.1:51786/ui?"), "got {url}");
        // A space and `/` in the root are percent/plus-encoded — the query survives the round-trip.
        assert!(url.contains("root=%2Ftmp%2Fmy+repo"), "root encoded: {url}");
        assert!(url.contains("format=html"), "{url}");
        assert!(url.contains("edges=all"), "{url}");
        assert!(url.contains("algorithm=label_propagation"), "{url}");
        assert!(url.contains("min_confidence=0.5"), "{url}");
        assert!(url.contains("max_nodes=200"), "{url}");
        assert!(url.contains("max_edges=700"), "{url}");
        assert!(url.contains("focus=src%2Fmcp"), "{url}");
        // Round-trips back through the route parser to the same knobs.
        let query = url.split_once('?').unwrap().1;
        let args = parse_ui_args(Some(query)).expect("route parses its own URL");
        assert_eq!(args.root, PathBuf::from("/tmp/my repo"));
        assert_eq!(args.format, "html");
        assert_eq!(args.min_confidence, Some(0.5));
        assert_eq!(args.max_nodes, Some(200));
        assert_eq!(args.max_edges, Some(700));
        assert_eq!(args.focus.as_deref(), Some("src/mcp"));
    }

    #[test]
    fn build_ui_url_omits_absent_optionals() {
        let addr: SocketAddr = "127.0.0.1:51786".parse().unwrap();
        let url = build_ui_url(
            &addr,
            Path::new("/repo"),
            "svg",
            "calls",
            "louvain",
            None,
            None,
            None,
            None,
        );
        assert!(url.contains("format=svg"), "{url}");
        assert!(!url.contains("min_confidence"), "{url}");
        assert!(!url.contains("max_nodes"), "{url}");
        assert!(!url.contains("max_edges"), "{url}");
        assert!(!url.contains("focus"), "{url}");
    }

    #[test]
    fn parse_ui_args_requires_root_and_defaults_knobs() {
        assert!(parse_ui_args(Some("format=html")).is_none(), "root is required");
        assert!(parse_ui_args(Some("root=%20%20")).is_none(), "blank root rejected");
        assert!(parse_ui_args(None).is_none());
        let args = parse_ui_args(Some("root=%2Frepo")).expect("root-only parses");
        assert_eq!(args.root, PathBuf::from("/repo"));
        assert_eq!(args.format, "html");
        assert_eq!(args.edges, "all");
        assert_eq!(args.algorithm, "label_propagation");
        assert_eq!(args.min_confidence, None);
        assert_eq!(args.max_nodes, None);
        assert_eq!(args.max_edges, None);
        assert_eq!(args.focus, None);
    }

    /// The served-path selection the `ui` tool depends on: with a daemon answering, `served_ui_url`
    /// hands back a live `http://<addr>/ui?…` URL; with only a stale portfile (crashed daemon) it
    /// degrades to `None` so the tool falls back to the `file://` export rather than a dead link.
    /// This is the one end-to-end check of the `served:true` path — every other `ui` test exercises
    /// only the no-daemon fallback.
    #[tokio::test]
    async fn served_ui_url_resolves_live_daemon_and_degrades_when_dead() {
        use std::net::TcpListener;

        let comms_dir = tempfile::tempdir().expect("comms tempdir");
        // `root` only needs to be an existing path (it is percent-encoded into the URL, not hosted).
        let root = comms_dir.path();

        // the backlog), so a URL targeting that address is returned.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener addr");
        write_portfile(comms_dir.path(), &addr).expect("write portfile");
        let live = served_ui_url_from_comms_dir(
            comms_dir.path(),
            root,
            "html",
            "all",
            "label_propagation",
            None,
            None,
            None,
            None,
        )
        .await
        .expect("a daemon answering the probe yields a served URL");
        assert!(
            live.starts_with(&format!("http://{addr}/ui?")),
            "served URL targets the live daemon address: {live}"
        );
        assert!(
            live.contains("format=html") && live.contains("edges=all"),
            "served URL carries the requested knobs: {live}"
        );
        drop(listener);

        // Dead: rewrite the portfile to an address nothing is listening on (bind + drop to grab an
        // almost-certainly-free port). The probe's connect fails, so the tool degrades to the export.
        let dead_addr = {
            let ephemeral = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
            ephemeral.local_addr().expect("listener addr")
        };
        write_portfile(comms_dir.path(), &dead_addr).expect("rewrite portfile");
        let dead = served_ui_url_from_comms_dir(
            comms_dir.path(),
            root,
            "html",
            "all",
            "label_propagation",
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            dead.is_none(),
            "a stale portfile with no daemon answering degrades to None, got {dead:?}"
        );
    }
}
