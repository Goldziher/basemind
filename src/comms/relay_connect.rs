//! Dialing the daemon's relay endpoint: the stream `basemind serve` relays an MCP session over, and
//! the one a CLI command forwards its call over.

use std::path::Path;

/// The client end of a relay link.
#[cfg(unix)]
pub type RelayStream = tokio::net::UnixStream;

/// The client end of a relay link.
#[cfg(windows)]
pub type RelayStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Dial the daemon's relay endpoint: a Unix-domain socket on unix.
#[cfg(unix)]
pub async fn connect_stream(socket_path: &Path) -> std::io::Result<RelayStream> {
    tokio::net::UnixStream::connect(socket_path).await
}

/// Dial the daemon's relay endpoint: a named pipe on Windows, retrying while the pipe is busy.
#[cfg(windows)]
pub async fn connect_stream(socket_path: &Path) -> std::io::Result<RelayStream> {
    const ERROR_PIPE_BUSY: i32 = 231;
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match super::pipe_owner::open_verified(socket_path) {
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
