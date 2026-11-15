//! `basemind serve`'s connection to the daemon: one relay session over the daemon's socket,
//! opened again whenever the daemon is replaced.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use basemind::comms::ids::AgentId;
use basemind::comms::{identity, relay, singleton};

use crate::relay_refusal::RelayRefusal;
use crate::stdio_relay::{self, StopPacing};

/// Keep the MCP host's stdio pipes alive across daemon replacement. The protocol-aware relay tracks
/// initialization and in-flight request IDs, allowing it to reconnect without asking Codex or
/// Claude to recreate the MCP process. The first connection goes through the relay as well, so a
/// daemon that cannot be reached when the session starts is answered for on the host's own pipe
/// instead of ending the process before `initialize` is read.
pub(crate) fn try_serve_relay(root: &Path, view: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let served = runtime.block_on(async move {
        let agent = identity::mcp_session_agent_id(root);
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let view = view.to_owned();
        let pacing = Arc::new(Mutex::new(StopPacing::default()));
        tracing::info!(
            pid = std::process::id(),
            view = %view,
            root = %canonical_root.display(),
            "basemind serve: persistent relay starting"
        );
        stdio_relay::run(tokio::io::stdin(), tokio::io::stdout(), move || {
            open_relay_connection(canonical_root.clone(), view.clone(), agent.clone(), Arc::clone(&pacing))
        })
        .await
        .context("run persistent daemon relay")
    });
    // The stdin reader is a blocking read on a thread of its own that cannot be cancelled. A relay
    // that ends while the host still holds its pipe open must not wait for that read to return.
    runtime.shutdown_background();
    served
}

#[cfg(unix)]
type RelayStream = tokio::net::UnixStream;

#[cfg(windows)]
type RelayStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Connect a relay to the daemon, bringing one up if needed. `pacing` decides whether this attempt
/// may ask an older daemon to make way, and is told when it did.
async fn open_relay_connection(
    root: PathBuf,
    view: String,
    agent: AgentId,
    pacing: Arc<Mutex<StopPacing>>,
) -> Result<RelayStream> {
    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let may_ask = pacing.lock().unwrap_or_else(PoisonError::into_inner).may_ask();
    let (ensured, stop_sent) = singleton::ensure_daemon_paced(&paths, may_ask).await;
    if stop_sent {
        pacing.lock().unwrap_or_else(PoisonError::into_inner).asked();
    }
    ensured.context("ensure daemon")?;
    let mut stream = relay_connect_stream(&paths.socket_path)
        .await
        .context("connect to daemon socket")?;
    let hello = relay::RelayHello {
        relay_proto_ver: relay::RELAY_PROTO_VER,
        root,
        view,
        agent,
    };
    let welcome = relay::client_handshake(&mut stream, &hello)
        .await
        .context("relay handshake")?;
    if welcome.relay_proto_ver != relay::RELAY_PROTO_VER {
        return Err(RelayRefusal::ProtoMismatch {
            daemon: welcome.relay_proto_ver,
            client: relay::RELAY_PROTO_VER,
            daemon_version: welcome.daemon_version,
        }
        .into());
    }
    if !welcome.accepted {
        return Err(RelayRefusal::Declined {
            code: welcome.code.unwrap_or_else(|| "unspecified".to_string()),
            detail: welcome.detail,
        }
        .into());
    }
    Ok(stream)
}

/// Dial the daemon's relay endpoint: a Unix-domain socket on unix.
#[cfg(unix)]
async fn relay_connect_stream(socket_path: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(socket_path).await
}

/// Dial the daemon's relay endpoint: a named pipe on Windows, retrying while the pipe is busy.
#[cfg(windows)]
async fn relay_connect_stream(socket_path: &Path) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_PIPE_BUSY: i32 = 231;
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match ClientOptions::new().open(socket_path) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            Err(source) => return Err(source),
        }
    }
}
