//! In-process front-end + link over tokio mpsc channels.
//!
//! Used for same-process embedding (a future `basemind serve` that hosts the broker inline)
//! and for tests, which need an end-to-end client↔broker round-trip without a real socket.
//! The link skips framing entirely and moves owned [`CommsRequest`] / [`CommsOut`] values.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::daemon::{Broker, LinkGuard, Session};
use super::protocol::{CommsOut, CommsRequest};
use super::transport::{CommsFrontend, CommsLink, PeerCred};

/// Buffer depth for the per-link channels. Generous enough that a slow reader does not
/// back-pressure the broker on the common path; overflow drops the slowest sink (see the
/// broker's `fan_out`).
const CHANNEL_DEPTH: usize = 256;

/// One end of an in-process link, handed to a client.
pub struct InProcClientLink {
    to_broker: mpsc::Sender<CommsRequest>,
    from_broker: mpsc::Receiver<CommsOut>,
}

impl InProcClientLink {
    /// Send a request to the broker.
    pub async fn send_request(&self, req: CommsRequest) -> std::io::Result<()> {
        self.to_broker
            .send(req)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broker gone"))
    }

    /// Receive the next frame from the broker (response or notification).
    pub async fn recv(&mut self) -> Option<CommsOut> {
        self.from_broker.recv().await
    }
}

/// The broker-side half of an in-process link.
struct InProcLink {
    from_client: mpsc::Receiver<CommsRequest>,
    to_client: mpsc::Sender<CommsOut>,
}

impl CommsLink for InProcLink {
    async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
        Ok(self.from_client.recv().await)
    }

    async fn send(&mut self, out: CommsOut) -> std::io::Result<()> {
        self.to_client
            .send(out)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client gone"))
    }

    fn peer_cred(&self) -> PeerCred {
        PeerCred {
            uid: Some(current_uid()),
            pid: Some(std::process::id()),
        }
    }
}

/// In-process front-end. Hands out client links via [`InProcFrontend::connect`]; each spawns a
/// broker-side task driving the link.
pub struct InProcFrontend {
    broker: Arc<Broker>,
}

impl InProcFrontend {
    /// Build a front-end over an existing broker.
    pub fn new(broker: Arc<Broker>) -> Self {
        Self { broker }
    }

    /// Open a new in-process client link and spawn its broker-side serve task. The returned
    /// [`InProcClientLink`] is the client's handle.
    pub fn connect(&self) -> InProcClientLink {
        let (to_broker, from_client) = mpsc::channel(CHANNEL_DEPTH);
        let (to_client, from_broker) = mpsc::channel(CHANNEL_DEPTH);
        let link = InProcLink {
            from_client,
            to_client: to_client.clone(),
        };
        let broker = self.broker.clone();
        let guard = broker.register_link();
        tokio::spawn(async move {
            serve_link(broker, link, to_client, guard).await;
        });
        InProcClientLink { to_broker, from_broker }
    }
}

impl CommsFrontend for InProcFrontend {
    async fn serve(self: Box<Self>, _broker: Arc<Broker>, mut shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
        let _ = shutdown.changed().await;
        Ok(())
    }
}

/// Drive one link: read requests, dispatch through the broker, write responses.
/// `link_tx` is the notification sink the broker registers for `Subscribe`; `guard` is the link's
/// refcount with the idle reaper, dropped when the session ends.
async fn serve_link(broker: Arc<Broker>, mut link: InProcLink, link_tx: mpsc::Sender<CommsOut>, guard: LinkGuard) {
    let _link_guard = guard;
    let mut session = Session::default();
    while let Ok(Some(req)) = link.recv().await {
        let resp = broker.handle(req, &mut session, &link_tx).await;
        if link.send(CommsOut::Response(resp)).await.is_err() {
            break;
        }
    }
}

fn current_uid() -> u32 {
    super::frontend_uds::daemon_uid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::ids::{AgentId, ThreadId};
    use crate::comms::protocol::{CommsRequest, CommsResponse, PROTO_VER};
    use crate::comms::store::CommsStore;

    async fn expect_response(link: &mut InProcClientLink) -> CommsResponse {
        loop {
            match link.recv().await.expect("frame") {
                CommsOut::Response(r) => return r,
                CommsOut::Notification(_) => continue,
            }
        }
    }

    async fn hello(link: &mut InProcClientLink, name: &str) {
        link.send_request(CommsRequest::Hello {
            agent: AgentId::parse(name).expect("agent"),
            proto_ver: PROTO_VER,
            remote: None,
            cwd: None,
        })
        .await
        .expect("hello");
        assert!(matches!(expect_response(link).await, CommsResponse::Welcome { .. }));
    }

    /// Start a thread with `writer` as creator and `members` (satisfying the ≥2-of-3 rule via
    /// subject + members), returning its id.
    async fn start_thread(link: &mut InProcClientLink, members: &[&str]) -> ThreadId {
        link.send_request(CommsRequest::ThreadStart {
            subject: Some("Team".to_string()),
            path: None,
            members: members.iter().map(|m| AgentId::parse(*m).expect("agent")).collect(),
        })
        .await
        .expect("start");
        match expect_response(link).await {
            CommsResponse::Thread(t) => t.id,
            other => panic!("expected Thread, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_links_post_and_read_history_and_inbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
        let broker = Arc::new(Broker::new(store));
        let frontend = InProcFrontend::new(broker.clone());

        let mut writer = frontend.connect();
        let mut reader = frontend.connect();
        hello(&mut writer, "writer").await;
        hello(&mut reader, "reader").await;

        let thread = start_thread(&mut writer, &["reader"]).await;

        writer
            .send_request(CommsRequest::ThreadPost {
                thread: thread.clone(),
                subject: "status".to_string(),
                tags: vec!["daily".to_string()],
                reply_to: None,
                body: b"all green".to_vec(),
            })
            .await
            .expect("post");
        let message_id = match expect_response(&mut writer).await {
            CommsResponse::Posted { message_id } => message_id,
            other => panic!("expected Posted, got {other:?}"),
        };

        reader
            .send_request(CommsRequest::ThreadHistory {
                thread: thread.clone(),
                cursor: None,
                limit: Some(10),
                since_micros: None,
            })
            .await
            .expect("history");
        match expect_response(&mut reader).await {
            CommsResponse::History { messages, .. } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].meta.subject, "status");
                assert_eq!(messages[0].meta.id, message_id);
                assert_eq!(messages[0].meta.body_len, "all green".len() as u32);
            }
            other => panic!("expected History, got {other:?}"),
        }

        reader
            .send_request(CommsRequest::GetBody {
                message_id: message_id.clone(),
            })
            .await
            .expect("get_body");
        match expect_response(&mut reader).await {
            CommsResponse::Body { body } => {
                assert_eq!(body.as_deref(), Some(b"all green".as_ref()))
            }
            other => panic!("expected Body, got {other:?}"),
        }

        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: true,
                since_micros: None,
            })
            .await
            .expect("inbox");
        match expect_response(&mut reader).await {
            CommsResponse::Inbox { messages, .. } => {
                assert_eq!(messages.len(), 1, "the posted message is unread for the reader");
                assert_eq!(messages[0].meta.subject, "status");
            }
            other => panic!("expected Inbox, got {other:?}"),
        }

        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
                since_micros: None,
            })
            .await
            .expect("inbox");
        match expect_response(&mut reader).await {
            CommsResponse::Inbox { messages, .. } => {
                assert!(messages.is_empty(), "mark_read should clear the inbox");
            }
            other => panic!("expected Inbox, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inbox_excludes_self_authored_but_history_keeps_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
        let broker = Arc::new(Broker::new(store));
        let frontend = InProcFrontend::new(broker.clone());

        let mut writer = frontend.connect();
        let mut reader = frontend.connect();
        hello(&mut writer, "author").await;
        hello(&mut reader, "other").await;

        let thread = start_thread(&mut writer, &["other"]).await;

        writer
            .send_request(CommsRequest::ThreadPost {
                thread: thread.clone(),
                subject: "mine".to_string(),
                tags: vec![],
                reply_to: None,
                body: b"self note".to_vec(),
            })
            .await
            .expect("post");
        let message_id = match expect_response(&mut writer).await {
            CommsResponse::Posted { message_id } => message_id,
            other => panic!("expected Posted, got {other:?}"),
        };

        writer
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
                since_micros: None,
            })
            .await
            .expect("inbox");
        match expect_response(&mut writer).await {
            CommsResponse::Inbox { messages, unread, .. } => {
                assert!(messages.is_empty(), "an agent's own post must not appear in its inbox");
                assert_eq!(unread, 0, "self-authored messages are not unread for the author");
            }
            other => panic!("expected Inbox, got {other:?}"),
        }

        writer
            .send_request(CommsRequest::ThreadHistory {
                thread: thread.clone(),
                cursor: None,
                limit: Some(10),
                since_micros: None,
            })
            .await
            .expect("history");
        match expect_response(&mut writer).await {
            CommsResponse::History { messages, .. } => {
                assert_eq!(messages.len(), 1, "history keeps self-authored messages");
                assert_eq!(messages[0].meta.id, message_id);
            }
            other => panic!("expected History, got {other:?}"),
        }

        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
                since_micros: None,
            })
            .await
            .expect("inbox");
        match expect_response(&mut reader).await {
            CommsResponse::Inbox { messages, .. } => {
                assert_eq!(messages.len(), 1, "a different agent sees the message");
                assert_eq!(messages[0].meta.subject, "mine");
            }
            other => panic!("expected Inbox, got {other:?}"),
        }
    }
}

/// `CommsClient::wait_inbox` exercised against a REAL broker over a Unix-socket front-end — the
/// in-process transport above moves owned request/response values and cannot carry a
/// [`CommsClient`](crate::comms::client::CommsClient), which speaks the framed socket protocol.
/// Isolated to a per-test tempdir + socket path, same as the other real-transport tests in this
/// crate (never the user's `comms.sock`).
#[cfg(all(test, feature = "comms", unix))]
mod wait_inbox_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::comms::client::CommsClient;
    use crate::comms::daemon::Broker;
    use crate::comms::frontend_uds::UdsFrontend;
    use crate::comms::ids::{AgentId, ThreadId};
    use crate::comms::singleton::CommsPaths;
    use crate::comms::store::CommsStore;
    use crate::comms::transport::CommsFrontend;

    /// Spin up an isolated UDS broker and two clients (`alice`, `bob`) sharing one thread (alice
    /// creator, bob member). The caller drives `alice` / `bob`, then sends `true` on the returned
    /// shutdown sender and awaits the serve task before the tempdir is dropped.
    async fn two_clients_in_a_thread() -> (
        CommsClient,
        CommsClient,
        ThreadId,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<std::io::Result<()>>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("c.sock");
        let paths = CommsPaths {
            comms_dir: dir.path().to_path_buf(),
            socket_path: socket_path.clone(),
        };
        let store = Arc::new(CommsStore::open(dir.path()).expect("open comms store"));
        let broker = Arc::new(Broker::new(store));
        let listener = {
            let std_listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
            std_listener.set_nonblocking(true).expect("nonblocking");
            tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let frontend = UdsFrontend::from_listener(listener, socket_path.clone());
        let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });

        let mut alice = CommsClient::connect(&paths, AgentId::parse("alice").expect("agent"), None, None)
            .await
            .expect("connect alice");
        let bob = CommsClient::connect(&paths, AgentId::parse("bob").expect("agent"), None, None)
            .await
            .expect("connect bob");

        let thread = alice
            .start_thread(
                Some("wait".to_string()),
                None,
                vec![AgentId::parse("bob").expect("agent")],
            )
            .await
            .expect("start thread")
            .id;

        (alice, bob, thread, shutdown_tx, serve, dir)
    }

    /// With nobody posting, `wait_inbox` returns `timed_out: true` close to (not well past) the
    /// requested timeout — proving the timeout path actually bounds the block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_inbox_times_out_returns_timed_out() {
        let (mut alice, _bob, _thread, shutdown_tx, serve, _dir) = two_clients_in_a_thread().await;

        let started = Instant::now();
        let (timed_out, rows, _unread, _next) = alice
            .wait_inbox(None, None, None, None, None, 100, Duration::from_millis(200))
            .await
            .expect("wait_inbox");
        let elapsed = started.elapsed();

        assert!(timed_out, "no post landed; the wait must time out");
        assert!(rows.is_empty(), "a timed-out wait returns no rows");
        assert!(
            elapsed < Duration::from_secs(1),
            "elapsed {elapsed:?} should be close to the 200ms timeout, not the test's own ceiling"
        );

        let _ = shutdown_tx.send(true);
        let _ = serve.await;
    }

    /// A message posted BEFORE `wait_inbox` is called must be picked up by the pre-subscribe
    /// immediate check and returned without blocking for anywhere near the timeout — pinning the
    /// "subscribe first, then check" short-circuit from the design brief.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_inbox_immediate_unread_returns_without_blocking() {
        let (mut alice, mut bob, thread, shutdown_tx, serve, _dir) = two_clients_in_a_thread().await;

        bob.post_message(
            thread.clone(),
            "already posted".to_string(),
            b"hi".to_vec(),
            vec![],
            None,
        )
        .await
        .expect("bob posts before alice waits");

        let started = Instant::now();
        let (timed_out, rows, _unread, _next) = alice
            .wait_inbox(None, None, None, None, None, 100, Duration::from_secs(30))
            .await
            .expect("wait_inbox");
        let elapsed = started.elapsed();

        assert!(!timed_out, "a pre-existing unread message must short-circuit the wait");
        assert_eq!(rows.len(), 1, "the pre-existing message is returned");
        assert_eq!(rows[0].meta.subject, "already posted");
        assert!(
            elapsed < Duration::from_millis(500),
            "the immediate pre-subscribe check must short-circuit, not block toward the 30s \
             timeout: {elapsed:?}"
        );

        let _ = shutdown_tx.send(true);
        let _ = serve.await;
    }

    /// A message posted AFTER `wait_inbox` has subscribed (on an otherwise-empty inbox) wakes the
    /// call promptly via the notification push, not by falling through to the timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_inbox_wakes_on_post_after_subscribe() {
        let (mut alice, mut bob, thread, shutdown_tx, serve, _dir) = two_clients_in_a_thread().await;

        let waiter = tokio::spawn(async move {
            let started = Instant::now();
            let result = alice
                .wait_inbox(None, None, None, None, None, 100, Duration::from_secs(30))
                .await
                .expect("wait_inbox");
            (result, started.elapsed())
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        bob.post_message(thread.clone(), "woke you up".to_string(), b"hi".to_vec(), vec![], None)
            .await
            .expect("bob posts while alice waits");

        let ((timed_out, rows, _unread, _next), elapsed) = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("wait_inbox task did not finish in time")
            .expect("wait_inbox task panicked");

        assert!(!timed_out, "a post after subscribing must wake the wait");
        assert_eq!(rows.len(), 1, "the woken page carries the new message");
        assert_eq!(rows[0].meta.subject, "woke you up");
        assert!(
            elapsed < Duration::from_secs(5),
            "the wake should be near-instant (~50ms), nowhere near the 30s timeout: {elapsed:?}"
        );

        let _ = shutdown_tx.send(true);
        let _ = serve.await;
    }
}
