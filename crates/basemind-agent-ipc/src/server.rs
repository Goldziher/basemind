//! The daemon half: bridge one accepted connection to an in-process engine.
//!
//! [`serve_connection`] is generic over any `impl AgentClient`. In practice the daemon builds an
//! in-process channel ([`in_proc_channel`](basemind_agent::in_proc_channel)), spawns the
//! [`Session`](basemind_agent::Session) on the [`EngineEndpoint`](basemind_agent::EngineEndpoint)
//! half, and hands the [`InProcAgentClient`](basemind_agent::InProcAgentClient) half here — so this
//! bridge is the socket-facing mirror of a UI: commands decoded from the socket go *into* the engine,
//! events from the engine go *out* to the socket.

use basemind_agent::{AgentClient, AgentCommand};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::error::IpcError;
use crate::frame::{codec, decode, encode};

/// Run the daemon accept loop: for every accepted connection, mint a fresh engine-facing client via
/// `make_client` and bridge it to the socket with [`serve_connection`] on its own task.
///
/// `make_client` is called once per connection — the daemon passes
/// `|| template.new_client()`([`InProcAgentClient::new_client`](basemind_agent::InProcAgentClient::new_client)),
/// so every attach shares one long-lived engine and the session outlives any single connection.
/// Returns only on an unrecoverable accept error; a per-connection error is logged and the loop
/// continues.
pub async fn serve<C, F>(listener: UnixListener, mut make_client: F) -> Result<(), IpcError>
where
    C: AgentClient,
    F: FnMut() -> C,
{
    loop {
        let (stream, _addr) = listener.accept().await?;
        let client = make_client();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, client).await {
                tracing::warn!(%error, "agent ipc: connection ended with an error");
            }
        });
    }
}

/// Bridge a single connected `stream` to `client` (an engine-facing [`AgentClient`]): forward the
/// engine's events out as msgpack frames, and feed decoded inbound command frames into the engine.
/// Returns when either side closes (the engine shuts down, or the peer disconnects).
///
/// The two directions share `client` in one `select!`: when the inbound branch fires, the pending
/// `next_event()` future is dropped before `send_command` runs, so the `&mut self`/`&self` borrows
/// never overlap — the same cancellation shape the in-process UI loop relies on. Dropping a pending
/// `next_event()` does not lose an event (the engine's broadcast keeps it queued).
pub async fn serve_connection<C: AgentClient>(stream: UnixStream, mut client: C) -> Result<(), IpcError> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, codec());
    let mut writer = FramedWrite::new(write_half, codec());

    loop {
        tokio::select! {
            event = client.next_event() => match event {
                Some(event) => writer.send(encode(&event)?).await?,
                // The engine shut down; close the socket. ~keep
                None => break,
            },
            frame = reader.next() => match frame {
                Some(Ok(frame)) => {
                    let command: AgentCommand = decode(&frame)?;
                    // A `Shutdown` from a front-end means "this UI is detaching", not "kill the
                    // shared engine": the daemon session must outlive any single connection, so close
                    // this connection without forwarding it. (In-process mode never reaches here — it
                    // drives the engine directly, where `Shutdown` correctly ends the session.) ~keep
                    if matches!(command, AgentCommand::Shutdown) {
                        break;
                    }
                    // The engine only errors here if it is already gone, in which case the next
                    // `next_event()` returns `None` and ends the loop; nothing to do on error. ~keep
                    let _ = client.send_command(command).await;
                }
                Some(Err(error)) => return Err(error.into()),
                // The peer disconnected; dropping `client` closes the engine's command channel. ~keep
                None => break,
            },
        }
    }
    Ok(())
}
