//! `basemind serve --lead` (ADR-0040): the stdio relay of `serve`, with the lead's two tools
//! answered in this process.
//!
//! A filter sits between the MCP host's stdio and [`crate::stdio_relay::run`], which runs exactly
//! as for a plain `serve`, over an in-memory pipe. From the host it takes a `tools/call` of
//! `agent_launch` / `agent_stop` and answers it here, never writing it to the relay, so neither
//! ever reaches the daemon. Every other line goes to the relay byte for byte. On the way back it
//! appends the two tools to the first page of a `tools/list` answer, and a one-line notice to the
//! `initialize` answer's `instructions`: what the lead may do, or why launching is off.
//!
//! A plain `serve` never builds this filter, so what it sends and lists is unchanged. When the lead
//! host cannot start (no operator policy, a refused one, no audit log, a nested process), the
//! filter still runs, adds only the notice, and passes the two tools' calls on to the daemon, which
//! answers them as unknown tools.
//!
//! Only what the filter recognises is changed: a line over [`MAX_INSPECTED_BYTES`], one that is
//! not UTF-8 or not a JSON object, and a JSON-RPC batch (an array) all pass through unchanged. A
//! lead tool called inside a batch therefore reaches the daemon and is answered as unknown, and a
//! `tools/list` inside a batch, or one asking for a later page (`cursor`), is not extended.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use basemind::mcp::lead_tools::{self, LEAD_TOOLS};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio::sync::mpsc;

/// The longest line the filter parses. Longer ones pass through untouched; no request of the lead
/// tools, and no `tools/list` or `initialize` answer, comes near it.
pub(crate) const MAX_INSPECTED_BYTES: usize = 1024 * 1024;
/// Capacity of the in-memory pipe to the relay, each way.
const PIPE_BYTES: usize = 256 * 1024;
/// Local answers waiting to be written to the host.
const LOCAL_ANSWERS: usize = 64;
/// How long a lead tool call still running when the relay ends may take to be answered.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

/// What answers the lead tools in this process.
pub(crate) trait LeadCalls: Send + Sync + 'static {
    /// The `CallToolResult` for `name` called with `arguments`.
    fn call(self: Arc<Self>, name: String, arguments: Value) -> Pin<Box<dyn Future<Output = Value> + Send>>;

    /// Stop what the calls started and wait for it; blocks.
    fn shutdown(&self);
}

impl LeadCalls for basemind::lead::LeadHost {
    fn call(self: Arc<Self>, name: String, arguments: Value) -> Pin<Box<dyn Future<Output = Value> + Send>> {
        Box::pin(async move { basemind::lead::LeadHost::call(self, &name, arguments).await })
    }

    fn shutdown(&self) {
        basemind::lead::LeadHost::shutdown(self);
    }
}

/// What the filter does for this session.
pub(crate) enum Mode {
    /// The lead host runs: the two tools are listed and answered here.
    On {
        /// Answers the calls.
        calls: Arc<dyn LeadCalls>,
        /// The `instructions` line.
        notice: String,
    },
    /// Launching is off: only the notice saying why is added.
    Off {
        /// The `instructions` line.
        notice: String,
    },
}

/// Seal this process before anything else if it is to be a lead: argv is read raw, before clap,
/// tracing or the runtime start, so no request can be written into its stdin through `/proc`
/// meanwhile. A process sealed this way that then falls back to a plain relay stays sealed: it
/// writes no core dump, and a debugger, `/proc/<pid>/environ` and `/proc/<pid>/fd` need root;
/// its logs on stderr are unaffected.
pub(crate) fn seal_if_lead() {
    if std::env::args_os().skip(1).any(|arg| arg == "--lead") {
        // The registration seals again and refuses (`host_dumpable`) if it did not take.
        let _ = basemind_agents::seal_host();
    }
}

/// The mode for the lead whose workspace is `root` and comms identity `principal`: the host, or the
/// reason it could not start, which is also written to stderr.
pub(crate) fn mode(root: &std::path::Path, principal: basemind::comms::v2::Principal, host: Option<String>) -> Mode {
    let started = basemind::lead::Setup::from_user()
        .and_then(|setup| basemind::lead::LeadHost::start(root, principal, host, setup));
    match started {
        Ok(lead) => {
            let notice = lead.notice();
            tracing::info!("basemind serve --lead: {notice}");
            Mode::On {
                calls: Arc::new(lead),
                notice,
            }
        }
        Err(off) => {
            tracing::error!("basemind serve --lead: {off}; serving without agent_launch");
            Mode::Off {
                notice: format!(
                    "{}; agent_launch and agent_stop are not available in this session (the reason is on \
                     the serve's stderr).",
                    off.1
                ),
            }
        }
    }
}

impl Mode {
    /// What answers the calls, when the lead host runs: the `serve` shuts it down once the
    /// filter has returned.
    pub(crate) fn calls(&self) -> Option<Arc<dyn LeadCalls>> {
        match self {
            Self::On { calls, .. } => Some(Arc::clone(calls)),
            Self::Off { .. } => None,
        }
    }
}

/// Resolves once the process is asked to end by SIGTERM, SIGHUP or SIGINT. A lead treats that as
/// the end of its input, so its sessions are stopped and recorded before it exits. Without a
/// handler the signal would end the process at once, leaving detached processes behind.
pub(crate) async fn termination() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("basemind serve --lead: cannot handle termination signals");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = hup.recv() => {}
        _ = int.recv() => {}
    }
}

/// Ids of the answers the filter changes on their way to the host.
#[derive(Default)]
struct Awaited {
    lists: HashSet<String>,
    initializes: HashSet<String>,
}

/// Run the filter between `input` / `output` (the host's stdio) and `relay`, which serves the other
/// end of an in-memory pipe exactly as it would serve the host. Returns when the relay does.
pub(crate) async fn run<R, W, F, Fut>(input: R, mut output: W, mode: Mode, relay: F) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
    F: FnOnce(tokio::io::ReadHalf<DuplexStream>, tokio::io::WriteHalf<DuplexStream>) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let (ours, theirs) = tokio::io::duplex(PIPE_BYTES);
    let (from_relay, to_relay) = tokio::io::split(ours);
    let (relay_read, relay_write) = tokio::io::split(theirs);
    let awaited = Arc::new(Mutex::new(Awaited::default()));
    let (answers, mut answered) = mpsc::channel::<String>(LOCAL_ANSWERS);
    let (calls, notice) = match mode {
        Mode::On { calls, notice } => (Some(calls), notice),
        Mode::Off { notice } => (None, notice),
    };
    let inbound = tokio::spawn(inbound(input, to_relay, calls, Arc::clone(&awaited), answers));
    let relayed = relay(relay_read, relay_write);
    let outbound = async {
        let mut from_relay = BufReader::new(from_relay);
        // `read_until` keeps what it read in `line` when the other branch wins, and goes on from
        // there: the buffer is cleared only once a whole line was written.
        let mut line = Vec::new();
        loop {
            tokio::select! {
                read = from_relay.read_until(b'\n', &mut line) => {
                    if read? == 0 {
                        output.write_all(&line).await?;
                        break;
                    }
                    let shown = outgoing(&line, &awaited, &notice);
                    output.write_all(shown.as_deref().unwrap_or(&line)).await?;
                    line.clear();
                }
                Some(mut answer) = answered.recv() => {
                    answer.push('\n');
                    output.write_all(answer.as_bytes()).await?;
                }
            }
            output.flush().await?;
        }
        // The relay is gone: read no more from the host, and give the lead calls already taken
        // their answers. The reader's sender goes with it; each call's goes when it is answered.
        inbound.abort();
        let _ = tokio::time::timeout(DRAIN, async {
            while let Some(mut answer) = answered.recv().await {
                answer.push('\n');
                output.write_all(answer.as_bytes()).await?;
                output.flush().await?;
            }
            io::Result::Ok(())
        })
        .await;
        output.flush().await
    };
    let (relayed, written) = tokio::join!(relayed, outbound);
    relayed.and(written)
}

/// Copy the host's lines to the relay, answering the lead tools' calls here instead.
async fn inbound<R>(
    input: R,
    mut to_relay: tokio::io::WriteHalf<DuplexStream>,
    calls: Option<Arc<dyn LeadCalls>>,
    awaited: Arc<Mutex<Awaited>>,
    answers: mpsc::Sender<String>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            return to_relay.shutdown().await;
        }
        match incoming(&line, calls.is_some(), &awaited) {
            Incoming::Forward => {
                to_relay.write_all(&line).await?;
                to_relay.flush().await?;
            }
            Incoming::Answer { id, name, arguments } => {
                let Some(calls) = calls.clone() else { continue };
                let answers = answers.clone();
                tokio::spawn(async move {
                    let result = calls.call(name, arguments).await;
                    let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    let _ = answers.send(response.to_string()).await;
                });
            }
        }
    }
}

enum Incoming {
    Forward,
    Answer { id: Value, name: String, arguments: Value },
}

/// What to do with one host line; records the ids of the answers [`outgoing`] changes.
fn incoming(line: &[u8], lead: bool, awaited: &Mutex<Awaited>) -> Incoming {
    let Some(Value::Object(message)) = inspect(line) else {
        return Incoming::Forward;
    };
    let Some(id) = message.get("id").filter(|id| !id.is_null()) else {
        return Incoming::Forward;
    };
    let key = id.to_string();
    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            awaited
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .initializes
                .insert(key);
        }
        Some("tools/list") if lead => {
            let paged = message
                .get("params")
                .and_then(|params| params.get("cursor"))
                .is_some_and(|cursor| !cursor.is_null());
            if !paged {
                awaited.lock().unwrap_or_else(PoisonError::into_inner).lists.insert(key);
            }
        }
        Some("tools/call") if lead => {
            let params = message.get("params");
            let name = params.and_then(|params| params.get("name")).and_then(Value::as_str);
            if let Some(name) = name.filter(|name| LEAD_TOOLS.contains(name)) {
                let arguments = params
                    .and_then(|params| params.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                return Incoming::Answer {
                    id: id.clone(),
                    name: name.to_owned(),
                    arguments,
                };
            }
        }
        _ => {}
    }
    Incoming::Forward
}

/// The line to write to the host in place of `line`, or `None` to write it as it is.
fn outgoing(line: &[u8], awaited: &Mutex<Awaited>, notice: &str) -> Option<Vec<u8>> {
    {
        let awaited = awaited.lock().unwrap_or_else(PoisonError::into_inner);
        if awaited.lists.is_empty() && awaited.initializes.is_empty() {
            return None;
        }
    }
    let Some(Value::Object(mut message)) = inspect(line) else {
        return None;
    };
    if message.contains_key("method") {
        return None;
    }
    let key = message.get("id")?.to_string();
    let (list, initialize) = {
        let mut awaited = awaited.lock().unwrap_or_else(PoisonError::into_inner);
        (awaited.lists.remove(&key), awaited.initializes.remove(&key))
    };
    let result = message.get_mut("result").and_then(Value::as_object_mut)?;
    if list {
        let Value::Array(tools) = result.get_mut("tools")? else {
            return None;
        };
        tools.extend(lead_tools::tool_definitions());
    } else if initialize {
        let instructions = match result.get("instructions").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => format!("{text}\n{notice}"),
            _ => notice.to_owned(),
        };
        result.insert("instructions".to_owned(), Value::String(instructions));
    } else {
        return None;
    }
    let mut changed = serde_json::to_vec(&Value::Object(message)).ok()?;
    changed.push(b'\n');
    Some(changed)
}

/// `line` as JSON, when it is short enough to inspect, UTF-8 and well-formed. serde_json bounds
/// nesting itself (128 levels) and reports deeper input as an error.
fn inspect(line: &[u8]) -> Option<Value> {
    if line.len() > MAX_INSPECTED_BYTES {
        return None;
    }
    let text = std::str::from_utf8(line).ok()?;
    serde_json::from_str(text.trim_end()).ok()
}

#[cfg(test)]
#[path = "serve_lead_tests.rs"]
mod tests;
