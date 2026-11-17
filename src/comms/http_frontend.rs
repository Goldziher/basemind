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
//! ## Opt-in + authentication
//!
//! This front-end does not exist unless an operator grants it, and every request must
//! present the daemon's bearer token. Both live in [`http_auth`](super::http_auth), which documents
//! why loopback alone was never an authentication boundary.
//!
//! ## Address + portfile
//!
//! The listener binds a loopback address: [`HTTP_ADDR_ENV`] when set, else `127.0.0.1:51786`. The
//! default is a preference, not a lock. Daemons of two protocol generations run side by side, and
//! the second one to start finds the default port taken; it binds a port the OS assigns instead.
//! An address set through [`HTTP_ADDR_ENV`] is binding: if it is in use, the front-end does not
//! start. The actually-bound `host:port` and the process's bearer token are written to
//! `<comms_dir>/http.addr` (mode 0600 on Unix), in the daemon's own generation directory, so
//! tooling — `basemind daemon ensure` and any launcher — discovers both without guessing the port.
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

use super::accept_gate;
use super::daemon::Broker;
use super::http_auth::{self, HttpToken};
use super::ids::AgentId;
use crate::mcp::BasemindServer;

pub use super::http_auth::{ALLOW_HTTP_ENV, http_enabled, portfile_path};

/// Env var overriding the bound loopback address. `host:port`, e.g. `127.0.0.1:0` to let the OS pick
/// a free port (the actual port is discoverable via the portfile). Empty/unset uses
/// [`DEFAULT_HTTP_ADDR`], or an OS-assigned port when that is in use.
pub const HTTP_ADDR_ENV: &str = "BASEMIND_HTTP_ADDR";
/// Preferred loopback address for the streamable-HTTP MCP transport.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:51786";
/// The MCP JSON-RPC transport path. Anything else is a 404.
const MCP_PATH: &str = "/mcp";
/// Poll cadence while [`await_http_ready`] waits for the listener to answer.
const HTTP_READY_POLL: Duration = Duration::from_millis(50);

/// Response body type produced by every path here — the shape [`StreamableHttpService::handle`]
/// returns, so the router and the service agree.
type HttpBody = BoxBody<Bytes, Infallible>;

/// The address to bind and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BindAddr {
    addr: SocketAddr,
    /// Set through [`HTTP_ADDR_ENV`], so it must be bound as given or not at all.
    configured: bool,
}

/// Resolve the address to bind: [`HTTP_ADDR_ENV`] when set and non-blank, else [`DEFAULT_HTTP_ADDR`].
fn resolve_addr() -> Result<BindAddr> {
    let configured = std::env::var(HTTP_ADDR_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let raw = configured.as_deref().unwrap_or(DEFAULT_HTTP_ADDR);
    let addr = raw
        .parse::<SocketAddr>()
        .with_context(|| format!("parse {HTTP_ADDR_ENV}={raw:?} as a host:port socket address"))?;
    Ok(BindAddr {
        addr,
        configured: configured.is_some(),
    })
}

/// Bind the HTTP listener. When the default address is in use — by the daemon of another protocol
/// generation, or by anything else — bind a port the OS assigns on the same interface instead; the
/// portfile carries the result. A configured address is never substituted.
async fn bind_http_listener(bind: BindAddr) -> std::io::Result<TcpListener> {
    match TcpListener::bind(bind.addr).await {
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && !bind.configured => {
            let any_port = SocketAddr::new(bind.addr.ip(), 0);
            tracing::warn!(
                addr = %bind.addr,
                "comms: the default streamable-HTTP address is in use (another basemind daemon?); \
                 binding an OS-assigned port instead, published in http.addr"
            );
            TcpListener::bind(any_port).await
        }
        bound => bound,
    }
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

/// Maximum approved roots memoized by [`resolve_request_root`]. The real population is the handful
/// of repos one daemon serves; the cap exists so a client spraying distinct `?root=` values cannot
/// grow the map without bound. Overflow clears wholesale rather than evicting an LRU victim — at
/// this size the rebuild costs one guard pass per live workspace.
const APPROVED_ROOT_CACHE_CAP: usize = 64;

/// Canonicalized `?root=` values the guard has already approved, mapped to the workspace root they
/// resolved to.
///
/// The streamable-HTTP transport is stateless (`with_legacy_session_mode(false)`), so every
/// JSON-RPC message is a fresh POST and [`resolve_request_root`] runs per *message*, not per cold
/// open — unlike [`WorkspacePool::get_or_open`](super::workspace_pool), which sits behind a map hit.
/// The guard's git test opens a `gix` repository each time (walk up for `.git`, then read
/// `.git/config`, `HEAD`, and the user/system gitconfigs): tens of syscalls of pure per-request
/// overhead.
///
/// Only APPROVALS are cached, which is what makes this sound: refusals are re-derived every time, and
/// a root's verdict otherwise only moves *toward* allowed (`basemind init`, an index appearing, the
/// grandfather record being written). The two ways it could move back — deleting the repo's `.git`,
/// or unsetting `BASEMIND_ALLOW_ANY_ROOT` — cannot happen inside one daemon process for a workspace
/// it is already serving, since the daemon's environment is fixed at spawn.
static APPROVED_ROOTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// A refusal from [`resolve_request_root`]: the status plus the plain-text body that explains it.
///
/// Deliberately the *parts* of a response rather than a built `Response<HttpBody>`: a whole hyper
/// response in the `Err` arm makes `Result` 128+ bytes wide on every call, which `clippy`'s
/// `result_large_err` rejects. The caller builds the response.
type RootRefusal = (StatusCode, std::borrow::Cow<'static, str>);

/// Resolve a `?root=` query value into a usable workspace root, or the parts of the refusal.
///
/// A relative path is rejected
/// *before* `canonicalize`, which would otherwise resolve it against the DAEMON's cwd — wherever
/// the daemon happened to be spawned — rather than anywhere the caller named. A non-project root
/// is refused here, upstream of the workspace pool, so that no cache directory is minted
/// for `/` and the caller is told what to do instead of getting a generic "could not be hosted"
/// (issue #62).
///
/// Discovery runs on the canonicalized path before the verdict so HTTP and the CLI agree on what a
/// root *is*: `discover_root_with_basemind` lifts a repo subdirectory to its workdir, which every
/// CLI verb already gets for free. Without it an HTTP caller naming `<repo>/src` would be refused
/// outright (the guard requires `workdir == root`) while the same path on the command line works.
fn resolve_request_root(raw_root: &Path) -> Result<PathBuf, RootRefusal> {
    if raw_root.is_relative() {
        return Err((
            StatusCode::BAD_REQUEST,
            "bad request: ?root= must be an absolute path".into(),
        ));
    }
    let Ok(canonical) = std::fs::canonicalize(raw_root) else {
        return Err((
            StatusCode::NOT_FOUND,
            "not found: root does not resolve to an existing path".into(),
        ));
    };
    {
        let approved = APPROVED_ROOTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(root) = approved.get(&canonical) {
            return Ok(root.clone());
        }
    }
    let discovered = crate::config::discover_root_with_basemind(&canonical);
    match crate::config::root_guard::workspace_root_verdict(&discovered) {
        Ok(root) => {
            let mut approved = APPROVED_ROOTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if approved.len() >= APPROVED_ROOT_CACHE_CAP {
                approved.clear();
            }
            approved.insert(canonical, root.clone());
            Ok(root)
        }
        Err(refusal) => Err((
            StatusCode::FORBIDDEN,
            crate::config::root_guard::refusal_message(&discovered, refusal).into(),
        )),
    }
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

/// Read and discard a rejected request's body before answering.
///
/// Closing a connection while unread data is still in the receive buffer makes the OS send RST
/// rather than FIN, and the client then fails its *read* with `ECONNRESET` instead of seeing the
/// response — so a caller with a bad token would get "connection reset by peer" rather than the 401
/// that tells it what to do. Rejecting before routing (which is where the check belongs) means
/// nothing downstream consumes the body, so the drain has to happen here.
///
/// Bounded, because an unauthenticated caller must not be able to make the daemon read as much as
/// it cares to send. Past the cap we stop and accept the reset: a request body larger than this is
/// not a client that merely got its token wrong.
async fn drain_request_body(request: &mut Request<Incoming>) {
    use http_body_util::BodyExt;

    /// Enough for any plausible MCP call that merely got its token wrong; far below anything worth
    /// reading for an attacker's benefit.
    const DRAIN_CAP: usize = 64 * 1024;

    let body = request.body_mut();
    let mut drained = 0usize;
    while drained < DRAIN_CAP {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    drained = drained.saturating_add(data.len());
                }
            }
            Some(Err(_)) | None => return,
        }
    }
}

/// The refusal for a request with no bearer token or the wrong one. Deliberately says nothing about
/// which workspaces exist or whether the presented value was close: an unauthenticated caller learns
/// only that this server wants a token.
fn unauthorized_response() -> Response<HttpBody> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(http::header::WWW_AUTHENTICATE, "Bearer")
        // ~keep A response built from constant parts is infallible; mirrors rmcp's own handlers.
        .body(
            Full::new(Bytes::from_static(
                b"unauthorized: present the daemon's bearer token as `Authorization: Bearer <token>`; \
                  it is the second line of <comms_dir>/http.addr",
            ))
            .boxed(),
        )
        .expect("unauthorized response builds from constant parts")
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
    /// This daemon process's bearer token. Every request on both routes must present it.
    token: HttpToken,
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
    async fn handle(&self, mut request: Request<Incoming>) -> Response<HttpBody> {
        // Bearer-token gate, FIRST — before routing, before the daemon is pinned, before a workspace
        // is named. Reaching this port proves only that a process on this machine opened a socket,
        // which is not an identity; the token is the identity. See `http_auth`.
        let presented = http_auth::presented_token(
            request
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        );
        if !presented.is_some_and(|token| self.token.matches(&token)) {
            drain_request_body(&mut request).await;
            return unauthorized_response();
        }

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

        if request.uri().path() != MCP_PATH {
            return text_response(StatusCode::NOT_FOUND, "not found: this server serves POST /mcp only");
        }
        let Some((raw_root, agent)) = parse_target(request.uri().query()) else {
            return text_response(StatusCode::NOT_FOUND, "not found: missing ?root=<abs-repo-path>");
        };
        let root = match resolve_request_root(&raw_root) {
            Ok(root) => root,
            Err((status, message)) => return text_response(status, &message),
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
            None => match super::v2::identity::resolve_for_root(&root) {
                Ok(resolved) => resolved.principal.as_str().to_string(),
                Err(_) => super::v2::identity::DEFAULT_HOST.to_string(),
            },
        };
        tracing::debug!(agent = %agent_id, root = %root.display(), "http: serving stateless mcp request");

        // The factory is called per request by rmcp; it has no access to the HTTP request, so the
        // root+agent binding is captured here and a fresh server is minted over the shared stack.
        let heavy_admission = Arc::clone(&self.broker.heavy_admission);
        let factory = move || {
            Ok(BasemindServer::from_shared(
                shared.clone(),
                agent_id.clone(),
                Arc::clone(&heavy_admission),
                None,
            ))
        };
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(self.cancel.child_token());
        let service = StreamableHttpService::new(factory, self.session_manager.clone(), config);
        service.handle(request).await
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

/// Serve the streamable-HTTP MCP transport until `shutdown` flips true (or its sender drops).
///
/// Returns `Ok(())` immediately, binding nothing, unless the operator granted the front-end via
/// [`ALLOW_HTTP_ENV`] — see [`http_auth`](super::http_auth) for why it is off by default. When it is
/// granted, this mints the process's bearer token, binds the loopback listener
/// ([`bind_http_listener`]), publishes address + token to the portfile, and serves each connection with a hyper auto (h1/h2)
/// server over the shared router. A bind failure is returned to the caller, which logs it and leaves
/// the rest of the daemon (the UDS relay) running — HTTP is additive, never a precondition for comms.
pub async fn serve_http(broker: Arc<Broker>, comms_dir: PathBuf, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    if !http_auth::http_enabled() {
        // A portfile from a previously-granted daemon would otherwise keep advertising an address
        // nothing is serving.
        http_auth::remove_portfile(&comms_dir);
        tracing::info!(
            grant = ALLOW_HTTP_ENV,
            "comms: streamable-HTTP MCP front-end not started (opt-in). The /mcp route is \
             unavailable; set {ALLOW_HTTP_ENV}=1 in the daemon's environment and restart it to enable \
             it.",
        );
        return Ok(());
    }
    let token = HttpToken::generate()?;
    let bind = resolve_addr()?;
    let addr = bind.addr;
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "BASEMIND_HTTP_ADDR binds a NON-loopback address: the MCP transport becomes reachable \
             off-host. rmcp Host-header (DNS-rebinding) validation still applies, but for genuine \
             remote access you must configure allowed_hosts/origins for the real hostnames."
        );
    }
    let listener = match bind_http_listener(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            // A portfile left by an earlier daemon of this generation would otherwise misdirect
            // `await_http_ready` / launchers / hard-coded manifests to whatever now holds that
            // address. Clear it before bailing so discovery fails cleanly. The portfile is in this
            // generation's directory, which only this generation's one daemon writes.
            http_auth::remove_portfile(&comms_dir);
            return Err(anyhow::Error::new(error)).with_context(|| {
                format!("bind streamable-HTTP MCP listener on {addr} (is another process holding it?)")
            });
        }
    };
    let local = listener.local_addr().context("read the bound HTTP address")?;
    http_auth::write_portfile(&comms_dir, &local, &token)
        .with_context(|| format!("write HTTP portfile under {}", comms_dir.display()))?;
    tracing::info!(addr = %local, "comms: streamable-HTTP MCP transport listening (bearer token required)");

    let cancel = CancellationToken::new();
    let router = Arc::new(HttpRouter {
        broker,
        session_manager: Arc::new(NeverSessionManager::default()),
        cancel: cancel.clone(),
        loopback: local.ip().is_loopback(),
        token,
    });

    let slots = accept_gate::connection_slots();
    let mut backoff = accept_gate::AcceptBackoff::new("http");
    loop {
        let slot = tokio::select! {
            changed = shutdown.changed() => {
                // A drain sends `true`; a dropped sender ends the daemon too. Either stops accepts.
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            slot = accept_gate::next_slot(&slots) => slot,
        };
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => {
                        backoff.succeeded();
                        pair
                    }
                    Err(error) => {
                        drop(slot);
                        backoff.failed(&error).await;
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
                    let _slot = slot;
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
    http_auth::remove_portfile(&comms_dir);
    tracing::info!("comms: streamable-HTTP MCP transport stopped");
    Ok(())
}

/// Poll until the transport answers a TCP connect (or `timeout` elapses), returning the bound
/// `host:port`. Used by `basemind daemon ensure` to confirm readiness before printing the base URL.
///
/// Deliberately does NOT consult [`ALLOW_HTTP_ENV`]: the grant that matters belongs to the *daemon*
/// process, and this runs in whatever process is doing the discovering. A caller that is about to
/// spawn the daemon from its own environment (`basemind daemon ensure`) can and does short-circuit on
/// the grant before calling this; a caller probing a daemon it did not spawn must go by the portfile.
pub async fn await_http_ready(comms_dir: &Path, timeout: Duration) -> Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(portfile) = http_auth::read_portfile(comms_dir)
            && tokio::net::TcpStream::connect(portfile.addr).await.is_ok()
        {
            return Ok(portfile.addr.to_string());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("streamable-HTTP MCP transport did not become ready within {timeout:?}");
        }
        tokio::time::sleep(HTTP_READY_POLL).await;
    }
}

/// The bearer token this machine's daemon published, for tooling that has to authenticate to the
/// transport (`basemind daemon ensure --json` hands it to whoever it printed the URL for).
pub fn published_token(comms_dir: &Path) -> Option<String> {
    http_auth::read_portfile(comms_dir)?.token
}

/// The MCP base URL for a bound `host:port`.
pub fn base_url(addr: &str) -> String {
    format!("http://{addr}{MCP_PATH}")
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
        let default = resolve_addr().expect("default parses");
        assert_eq!(default.addr.to_string(), DEFAULT_HTTP_ADDR);
        assert!(!default.configured);
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "127.0.0.1:0") };
        assert_eq!(
            resolve_addr().expect("env parses"),
            BindAddr {
                addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                configured: true,
            }
        );
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
    }

    /// Two generations' daemons both prefer the default port. The second must still get a
    /// listener, on a port of its own, while an address the operator chose is never swapped.
    #[tokio::test]
    async fn a_default_address_in_use_falls_back_to_a_free_port_and_a_configured_one_does_not() {
        let holder = TcpListener::bind("127.0.0.1:0").await.expect("hold a port");
        let taken = holder.local_addr().expect("held address");

        let fallback = bind_http_listener(BindAddr {
            addr: taken,
            configured: false,
        })
        .await
        .expect("the default address being in use must not cost the front-end");
        let bound = fallback.local_addr().expect("fallback address");
        assert_ne!(bound.port(), taken.port());
        assert_eq!(bound.ip(), taken.ip(), "the fallback stays on the same interface");

        let refused = bind_http_listener(BindAddr {
            addr: taken,
            configured: true,
        })
        .await
        .expect_err("a configured address is bound as given or not at all");
        assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn base_url_appends_mcp_path() {
        assert_eq!(base_url("127.0.0.1:51786"), "http://127.0.0.1:51786/mcp");
    }
}
