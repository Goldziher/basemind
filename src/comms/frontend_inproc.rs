//! In-process front-end + link over tokio mpsc channels.
//!
//! Used for same-process embedding (a future `basemind serve` that hosts the broker inline)
//! and for tests, which need an end-to-end client↔broker round-trip without a real socket.
//! The link skips framing entirely and moves owned [`CommsRequest`] / [`CommsOut`] values.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::daemon::Broker;
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

    /// Close the request side and keep reading replies: a half-closed socket.
    #[cfg(test)]
    fn half_close(self) -> mpsc::Receiver<CommsOut> {
        self.from_broker
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
        let link = InProcLink { from_client, to_client };
        let broker = self.broker.clone();
        let guard = broker.register_link();
        tokio::spawn(super::transport::serve_link(broker, link, guard));
        InProcClientLink { to_broker, from_broker }
    }
}

impl CommsFrontend for InProcFrontend {
    async fn serve(self: Box<Self>, _broker: Arc<Broker>, mut shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
        let _ = shutdown.changed().await;
        Ok(())
    }
}

fn current_uid() -> u32 {
    super::frontend_uds::daemon_uid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::protocol::{CommsResponse, PROTO_VER};
    use crate::comms::v2::api::{InboxBody, PostBody, PostTarget, Principal};

    async fn expect_response(link: &mut InProcClientLink) -> CommsResponse {
        loop {
            match link.recv().await.expect("frame") {
                CommsOut::Response(r) | CommsOut::Reply { response: r, .. } => return r,
                CommsOut::Notification(_) => continue,
            }
        }
    }

    async fn hello(link: &mut InProcClientLink, name: &str, root: &std::path::Path) {
        link.send_request(CommsRequest::Hello {
            proto_ver: PROTO_VER,
            principal: Some(Principal::parse(name).expect("principal")),
            root: Some(root.to_path_buf()),
            host: None,
        })
        .await
        .expect("hello");
        assert!(matches!(expect_response(link).await, CommsResponse::Welcome { .. }));
    }

    #[tokio::test]
    async fn two_links_post_and_read_over_the_in_process_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = Arc::new(Broker::new(crate::comms::agents_hub::AgentHub::open(dir.path())));
        let frontend = InProcFrontend::new(broker);
        let mut writer = frontend.connect();
        let mut reader = frontend.connect();
        hello(&mut writer, "writer", dir.path()).await;
        hello(&mut reader, "reader", dir.path()).await;

        writer
            .send_request(CommsRequest::AgentPost {
                sub: None,
                body: PostBody {
                    target: PostTarget::NewThread {
                        subject: "status".to_string(),
                        to: vec![crate::comms::v2::api::Actor::parse("reader").expect("actor")],
                        paths: Vec::new(),
                    },
                    body: "all green".to_string(),
                },
            })
            .await
            .expect("post");
        let posted = expect_response(&mut writer).await;
        assert!(matches!(posted, CommsResponse::AgentPosted(_)), "{posted:?}");

        reader
            .send_request(CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 0,
            })
            .await
            .expect("inbox");
        match expect_response(&mut reader).await {
            CommsResponse::AgentRead(reply) => {
                assert_eq!(reply.messages.len(), 1);
                assert_eq!(reply.messages[0].body, "all green");
                assert_eq!(reply.messages[0].from.as_str(), "writer");
            }
            other => panic!("expected AgentRead, got {other:?}"),
        }
    }

    /// A client that pipelines a request behind a waiting read and then hangs up is noticed: the
    /// wait's client is gone, so a post that arrives afterwards is not marked read for it.
    #[tokio::test]
    async fn a_hang_up_behind_a_pipelined_request_ends_the_wait_unmarked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = Arc::new(Broker::new(crate::comms::agents_hub::AgentHub::open(dir.path())));
        let frontend = InProcFrontend::new(broker);
        let mut writer = frontend.connect();
        let mut reader = frontend.connect();
        hello(&mut writer, "writer", dir.path()).await;
        hello(&mut reader, "reader", dir.path()).await;
        let to_reader = |body: &str, target: PostTarget| CommsRequest::AgentPost {
            sub: None,
            body: PostBody {
                target,
                body: body.to_string(),
            },
        };
        writer
            .send_request(to_reader(
                "first",
                PostTarget::NewThread {
                    subject: "status".to_string(),
                    to: vec![crate::comms::v2::api::Actor::parse("reader").expect("actor")],
                    paths: Vec::new(),
                },
            ))
            .await
            .expect("post");
        let thread = match expect_response(&mut writer).await {
            CommsResponse::AgentPosted(reply) => reply.thread,
            other => panic!("{other:?}"),
        };
        let read_now = CommsRequest::AgentInbox {
            sub: None,
            body: InboxBody::default(),
            wait_secs: 0,
        };
        reader.send_request(read_now.clone()).await.expect("inbox");
        expect_response(&mut reader).await;

        reader
            .send_request(CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 30,
            })
            .await
            .expect("waiting inbox");
        reader.send_request(CommsRequest::Ping).await.expect("pipelined ping");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(reader);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        writer
            .send_request(to_reader("unseen", PostTarget::Thread(thread)))
            .await
            .expect("post");
        expect_response(&mut writer).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut again = frontend.connect();
        hello(&mut again, "reader", dir.path()).await;
        again.send_request(read_now).await.expect("inbox");
        match expect_response(&mut again).await {
            CommsResponse::AgentRead(reply) => {
                assert_eq!(reply.messages.len(), 1, "the message stayed unread");
                assert_eq!(reply.messages[0].body, "unseen");
            }
            other => panic!("expected AgentRead, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod half_close_tests {
    use super::*;
    use crate::comms::protocol::{CommsResponse, PROTO_VER};
    use crate::comms::v2::api::{InboxBody, Principal};

    /// A client that pipelines requests and then closes its write side still gets every reply.
    #[tokio::test]
    async fn requests_queued_before_a_half_close_are_still_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = Arc::new(Broker::new(crate::comms::agents_hub::AgentHub::open(dir.path())));
        let frontend = InProcFrontend::new(broker);
        let link = frontend.connect();
        link.send_request(CommsRequest::Hello {
            proto_ver: PROTO_VER,
            principal: Some(Principal::parse("reader").expect("principal")),
            root: Some(dir.path().to_path_buf()),
            host: None,
        })
        .await
        .expect("hello");
        link.send_request(CommsRequest::AgentInbox {
            sub: None,
            body: InboxBody::default(),
            wait_secs: 5,
        })
        .await
        .expect("waiting inbox");
        link.send_request(CommsRequest::Ping).await.expect("ping");
        link.send_request(CommsRequest::Ping).await.expect("ping");
        let mut replies = link.half_close();
        let mut answered = Vec::new();
        while let Some(frame) = tokio::time::timeout(std::time::Duration::from_secs(10), replies.recv())
            .await
            .expect("a reply or the end")
        {
            match frame {
                CommsOut::Response(response) | CommsOut::Reply { response, .. } => answered.push(response),
                CommsOut::Notification(_) => {}
            }
        }
        assert_eq!(answered.len(), 4, "{answered:?}");
        assert!(matches!(answered[0], CommsResponse::Welcome { .. }));
        assert!(matches!(answered[1], CommsResponse::AgentRead(_)), "{:?}", answered[1]);
        assert!(matches!(answered[2], CommsResponse::Pong), "{:?}", answered[2]);
        assert!(matches!(answered[3], CommsResponse::Pong), "{:?}", answered[3]);
    }
}

/// The daemon-side wait over the framed socket protocol, end to end through a
/// [`CommsClient`](crate::comms::client::CommsClient). Isolated to a per-test tempdir and socket.
#[cfg(all(test, feature = "comms", unix))]
mod wait_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::comms::agents_hub::AgentHub;
    use crate::comms::client::{ClientHello, CommsClient};
    use crate::comms::daemon::Broker;
    use crate::comms::frontend_uds::UdsFrontend;
    use crate::comms::singleton::CommsPaths;
    use crate::comms::transport::CommsFrontend;
    use crate::comms::v2::api::{Actor, InboxBody, PostBody, PostTarget, Principal, ThreadId, WaitOutcome};

    struct Daemon {
        paths: CommsPaths,
        shutdown: tokio::sync::watch::Sender<bool>,
        serve: tokio::task::JoinHandle<std::io::Result<()>>,
        dir: tempfile::TempDir,
    }

    impl Daemon {
        fn start() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("c.sock");
            let paths = CommsPaths {
                comms_dir: dir.path().to_path_buf(),
                socket_path: socket_path.clone(),
            };
            let broker = Arc::new(Broker::new(AgentHub::open(dir.path())));
            let listener = {
                let std_listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind temp socket");
                std_listener.set_nonblocking(true).expect("nonblocking");
                tokio::net::UnixListener::from_std(std_listener).expect("adopt listener")
            };
            let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
            let frontend = UdsFrontend::from_listener(listener, socket_path);
            let serve = tokio::spawn(async move { Box::new(frontend).serve(broker, shutdown_rx).await });
            Self {
                paths,
                shutdown,
                serve,
                dir,
            }
        }

        async fn client(&self, name: &str) -> CommsClient {
            let hello = ClientHello::agent(
                Principal::parse(name).expect("principal"),
                self.dir.path().to_path_buf(),
                None,
            );
            CommsClient::connect(&self.paths, hello).await.expect("connect")
        }

        async fn stop(self) {
            let _ = self.shutdown.send(true);
            let _ = self.serve.await;
        }
    }

    fn post_to(thread: &ThreadId, body: &str) -> PostBody {
        PostBody {
            target: PostTarget::Thread(thread.clone()),
            body: body.to_string(),
        }
    }

    async fn shared_thread(alice: &mut CommsClient, bob: &mut CommsClient) -> ThreadId {
        let created = alice
            .agent_post(
                None,
                PostBody {
                    target: PostTarget::NewThread {
                        subject: "wait".to_string(),
                        to: vec![Actor::parse("bob").expect("actor")],
                        paths: Vec::new(),
                    },
                    body: "first".to_string(),
                },
            )
            .await
            .expect("start thread");
        bob.agent_inbox(None, InboxBody::default(), 0)
            .await
            .expect("bob reads the first post");
        created.thread
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wait_over_the_socket_times_out_close_to_its_bound() {
        let daemon = Daemon::start();
        let (mut alice, mut bob) = (daemon.client("alice").await, daemon.client("bob").await);
        shared_thread(&mut alice, &mut bob).await;

        let started = Instant::now();
        let reply = bob.agent_inbox(None, InboxBody::default(), 1).await.expect("wait");
        let elapsed = started.elapsed();

        assert_eq!(reply.wait, WaitOutcome::TimedOut);
        assert!(reply.messages.is_empty());
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );
        daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wait_over_the_socket_ends_on_a_post() {
        let daemon = Daemon::start();
        let (mut alice, mut bob) = (daemon.client("alice").await, daemon.client("bob").await);
        let thread = shared_thread(&mut alice, &mut bob).await;

        let waiting = tokio::spawn(async move { bob.agent_inbox(None, InboxBody::default(), 30).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        alice.agent_post(None, post_to(&thread, "now")).await.expect("post");

        let reply = waiting.await.expect("waiter").expect("read");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(reply.wait, WaitOutcome::Arrived);
        assert_eq!(reply.messages.len(), 1);
        assert_eq!(reply.messages[0].body, "now");
        daemon.stop().await;
    }

    /// A waiting read whose client hangs up marks nothing read: the message stays for the next read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_hangs_up_during_a_wait_loses_nothing() {
        let daemon = Daemon::start();
        let (mut alice, mut bob) = (daemon.client("alice").await, daemon.client("bob").await);
        let thread = shared_thread(&mut alice, &mut bob).await;

        let waiting = tokio::spawn(async move { bob.agent_inbox(None, InboxBody::default(), 30).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        waiting.abort();
        let _ = waiting.await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        alice.agent_post(None, post_to(&thread, "kept")).await.expect("post");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut bob = daemon.client("bob").await;
        let reply = bob.agent_inbox(None, InboxBody::default(), 0).await.expect("read");
        assert_eq!(reply.messages.len(), 1, "the abandoned wait marked nothing read");
        assert_eq!(reply.messages[0].body, "kept");
        daemon.stop().await;
    }
}
