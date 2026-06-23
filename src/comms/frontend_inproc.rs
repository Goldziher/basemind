//! In-process front-end + link over tokio mpsc channels.
//!
//! Used for same-process embedding (a future `basemind serve` that hosts the broker inline)
//! and for tests, which need an end-to-end client↔broker round-trip without a real socket.
//! The link skips framing entirely and moves owned [`CommsRequest`] / [`CommsOut`] values.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::daemon::{Broker, Session};
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
        // Same process — trusted by construction.
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
        tokio::spawn(async move {
            serve_link(broker, link, to_client).await;
        });
        InProcClientLink {
            to_broker,
            from_broker,
        }
    }
}

impl CommsFrontend for InProcFrontend {
    async fn serve(
        self: Box<Self>,
        _broker: Arc<Broker>,
        mut shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        // The in-process front-end is driven by explicit `connect()` calls, so `serve` just
        // parks until shutdown — it exists to satisfy the trait for symmetry with the UDS
        // front-end.
        let _ = shutdown.changed().await;
        Ok(())
    }
}

/// Drive one link: read requests, dispatch through the broker, write responses.
/// `link_tx` is the notification sink the broker registers for `Subscribe`.
async fn serve_link(broker: Arc<Broker>, mut link: InProcLink, link_tx: mpsc::Sender<CommsOut>) {
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
    use crate::comms::ids::{AgentId, RoomId};
    use crate::comms::model::RoomScope;
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

    #[tokio::test]
    async fn two_links_post_and_read_history_and_inbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(CommsStore::open(dir.path()).expect("store"));
        let broker = Arc::new(Broker::new(store));
        let frontend = InProcFrontend::new(broker.clone());

        let mut writer = frontend.connect();
        let mut reader = frontend.connect();

        // Both say hello (Global default room).
        for (link, name) in [(&mut writer, "writer"), (&mut reader, "reader")] {
            link.send_request(CommsRequest::Hello {
                agent: AgentId::parse(name).expect("agent"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
                session_id: None,
                parent_agent: None,
            })
            .await
            .expect("hello");
            assert!(matches!(
                expect_response(link).await,
                CommsResponse::Welcome { .. }
            ));
        }

        // Create a shared global room and have the reader subscribe + join.
        let room = RoomId::parse("team").expect("room");
        writer
            .send_request(CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Global,
                title: Some("Team".to_string()),
            })
            .await
            .expect("create");
        assert!(matches!(
            expect_response(&mut writer).await,
            CommsResponse::Room(_)
        ));

        reader
            .send_request(CommsRequest::Join { room: room.clone() })
            .await
            .expect("join");
        assert!(matches!(
            expect_response(&mut reader).await,
            CommsResponse::Ok
        ));

        // Writer posts.
        writer
            .send_request(CommsRequest::Post {
                room: room.clone(),
                subject: "status".to_string(),
                tags: vec!["daily".to_string()],
                reply_to: None,
                scope: vec![],
                body: b"all green".to_vec(),
            })
            .await
            .expect("post");
        let posted = expect_response(&mut writer).await;
        let message_id = match posted {
            CommsResponse::Posted { message_id } => message_id,
            other => panic!("expected Posted, got {other:?}"),
        };

        // Reader reads history → sees the front-matter (not the body).
        reader
            .send_request(CommsRequest::History {
                room: room.clone(),
                cursor: None,
                limit: Some(10),
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

        // Reader fetches the body on demand.
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

        // Reader's inbox shows the unread message, then mark_read clears it.
        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: true,
            })
            .await
            .expect("inbox");
        match expect_response(&mut reader).await {
            CommsResponse::Inbox { messages, .. } => {
                assert_eq!(
                    messages.len(),
                    1,
                    "the posted message is unread for the reader"
                );
                assert_eq!(messages[0].meta.subject, "status");
            }
            other => panic!("expected Inbox, got {other:?}"),
        }

        // Second inbox read after mark_read → empty.
        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
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

        for (link, name) in [(&mut writer, "author"), (&mut reader, "other")] {
            link.send_request(CommsRequest::Hello {
                agent: AgentId::parse(name).expect("agent"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: None,
                session_id: None,
                parent_agent: None,
            })
            .await
            .expect("hello");
            assert!(matches!(
                expect_response(link).await,
                CommsResponse::Welcome { .. }
            ));
        }

        // A global room both agents auto-join (Global scope matches every chain).
        let room = RoomId::parse("team").expect("room");
        writer
            .send_request(CommsRequest::CreateRoom {
                room: room.clone(),
                scope: RoomScope::Global,
                title: Some("Team".to_string()),
            })
            .await
            .expect("create");
        assert!(matches!(
            expect_response(&mut writer).await,
            CommsResponse::Room(_)
        ));

        // The author posts to the room.
        writer
            .send_request(CommsRequest::Post {
                room: room.clone(),
                subject: "mine".to_string(),
                tags: vec![],
                reply_to: None,
                scope: vec![],
                body: b"self note".to_vec(),
            })
            .await
            .expect("post");
        let message_id = match expect_response(&mut writer).await {
            CommsResponse::Posted { message_id } => message_id,
            other => panic!("expected Posted, got {other:?}"),
        };

        // The author's OWN inbox excludes the message (auto-join subscribed them to the room).
        writer
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
            })
            .await
            .expect("inbox");
        match expect_response(&mut writer).await {
            CommsResponse::Inbox {
                messages, unread, ..
            } => {
                assert!(
                    messages.is_empty(),
                    "an agent's own post must not appear in its inbox"
                );
                assert_eq!(
                    unread, 0,
                    "self-authored messages are not unread for the author"
                );
            }
            other => panic!("expected Inbox, got {other:?}"),
        }

        // But room_history still shows it for the author — the full log is not filtered.
        writer
            .send_request(CommsRequest::History {
                room: room.clone(),
                cursor: None,
                limit: Some(10),
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

        // A different agent DOES see the message in their inbox.
        reader
            .send_request(CommsRequest::Inbox {
                remote: None,
                cwd: None,
                cursor: None,
                limit: Some(10),
                mark_read: false,
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
