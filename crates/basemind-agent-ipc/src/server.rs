//! The daemon half: bridge one accepted connection to an in-process engine.
//!
//! [`serve_connection`] is generic over any `impl AgentClient`. In practice the daemon builds an
//! in-process channel ([`in_proc_channel`](basemind_agent::in_proc_channel)), spawns the
//! [`Session`](basemind_agent::Session) on the [`EngineEndpoint`](basemind_agent::EngineEndpoint)
//! half, and hands the [`InProcAgentClient`](basemind_agent::InProcAgentClient) half here — so this
//! bridge is the socket-facing mirror of a UI: commands decoded from the socket go *into* the engine,
//! events from the engine go *out* to the socket.

use basemind_agent::AgentClient;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::error::IpcError;
use crate::frame::{codec, decode, encode};

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
                    let command = decode(&frame)?;
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
