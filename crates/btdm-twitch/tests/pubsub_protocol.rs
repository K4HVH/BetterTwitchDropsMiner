//! The PubSub protocol, against a real websocket server.
//!
//! Push is how the miner hears about progress, claims and streams ending
//! within seconds instead of minutes. Every failure mode here is one that
//! produces a socket which looks perfectly healthy and delivers nothing —
//! which is indistinguishable, from the outside, from a Twitch that has gone
//! quiet.

use btdm_twitch::pubsub::{self, PubSubMessage, Topic};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// What the fake PubSub saw, and a way to push to whoever is connected.
#[derive(Default)]
struct Server {
    frames: std::sync::Mutex<Vec<Value>>,
    connections: AtomicUsize,
}

impl Server {
    fn listens(&self) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f.get("type").and_then(Value::as_str) == Some("LISTEN"))
            .cloned()
            .collect()
    }

    fn topics_seen(&self) -> Vec<String> {
        self.listens()
            .iter()
            .filter_map(|f| f.get("data")?.get("topics")?.as_array().cloned())
            .flatten()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect()
    }
}

/// Behaviour the fake can be told to adopt.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Acknowledge, then push a drop-progress message.
    Normal,
    /// Reject the subscription, as Twitch does for a bad token.
    RejectListen,
    /// Accept the socket and then say nothing at all, ever.
    Silent,
    /// Acknowledge, then hang up.
    HangUp,
}

async fn fake_pubsub(server: Arc<Server>, mode: Mode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let server = Arc::clone(&server);

            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                server.connections.fetch_add(1, Ordering::SeqCst);
                let (mut write, mut read) = ws.split();

                while let Some(Ok(msg)) = read.next().await {
                    let Message::Text(text) = msg else { continue };
                    let Ok(frame): Result<Value, _> = serde_json::from_str(&text) else {
                        continue;
                    };
                    server.frames.lock().unwrap().push(frame.clone());

                    match frame.get("type").and_then(Value::as_str) {
                        Some("PING") => {
                            let _ = write.send(Message::Text(r#"{"type":"PONG"}"#.into())).await;
                        }
                        Some("LISTEN") => {
                            if mode == Mode::Silent {
                                continue;
                            }
                            let error = if mode == Mode::RejectListen {
                                "ERR_BADAUTH"
                            } else {
                                ""
                            };
                            let ack = json!({ "type": "RESPONSE", "nonce": "btdm", "error": error });
                            let _ = write.send(Message::Text(ack.to_string().into())).await;

                            if mode == Mode::HangUp {
                                let _ = write.close().await;
                                return;
                            }
                            if mode == Mode::Normal {
                                // A drop-progress push, as Twitch sends it.
                                let payload = json!({
                                    "type": "drop-progress",
                                    "data": {
                                        "drop_id": "d1",
                                        "current_progress_min": 23,
                                        "required_progress_min": 60,
                                    }
                                });
                                let envelope = json!({
                                    "type": "MESSAGE",
                                    "data": {
                                        "topic": "user-drop-events.1",
                                        "message": payload.to_string(),
                                    }
                                });
                                let _ = write
                                    .send(Message::Text(envelope.to_string().into()))
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });

    format!("ws://{addr}")
}

#[tokio::test]
async fn a_subscription_is_acknowledged_and_messages_arrive() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::Normal).await;

    let mut conn = pubsub::connect(&url, "token", vec![Topic::UserDrops.as_str("1")])
        .await
        .expect("a healthy server must connect");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), conn.recv())
        .await
        .expect("a pushed message must arrive")
        .expect("and not be a closed stream");

    match msg {
        PubSubMessage::DropProgress(p) => {
            assert_eq!(p.current_minutes, 23);
            assert_eq!(p.required_minutes, 60);
        }
        other => panic!("expected progress, got {other:?}"),
    }

    assert_eq!(server.topics_seen(), vec!["user-drop-events.1".to_string()]);
}

/// A rejected LISTEN leaves a socket that answers PING forever and delivers
/// nothing — indistinguishable from a healthy one unless the acknowledgement
/// is actually checked.
#[tokio::test]
async fn a_rejected_subscription_fails_the_connection() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::RejectListen).await;

    let result = pubsub::connect(&url, "bad-token", vec![Topic::UserDrops.as_str("1")]).await;

    let err = result.err().expect("a rejected subscription is not a connection");
    assert!(
        format!("{err}").contains("ERR_BADAUTH"),
        "the reason Twitch gave must survive: {err}"
    );
}

/// A server that accepts and then says nothing must not leave the miner
/// waiting forever believing push is working.
#[tokio::test]
async fn a_server_that_never_acknowledges_times_out() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::Silent).await;

    let started = std::time::Instant::now();
    let result = pubsub::connect(&url, "token", vec![Topic::UserDrops.as_str("1")]).await;

    assert!(result.is_err(), "silence is not an acknowledgement");
    assert!(
        started.elapsed() < pubsub::LISTEN_ACK_TIMEOUT + std::time::Duration::from_secs(2),
        "it waited {:?}, well past its own timeout",
        started.elapsed()
    );
}

/// The consumer must learn the socket is gone rather than simply never hearing
/// from it again — that is what lets the supervisor reconnect.
#[tokio::test]
async fn a_dropped_connection_is_reported() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::HangUp).await;

    let mut conn = pubsub::connect(&url, "token", vec![Topic::UserDrops.as_str("1")])
        .await
        .expect("it acknowledges before hanging up");

    let mut saw_disconnect = false;
    for _ in 0..5 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), conn.recv()).await {
            Ok(Some(PubSubMessage::Disconnected)) => {
                saw_disconnect = true;
                break;
            }
            // The stream ending is equally clear: the consumer knows.
            Ok(None) => {
                saw_disconnect = true;
                break;
            }
            Ok(Some(_)) => continue,
            Err(_) => break,
        }
    }
    assert!(saw_disconnect, "a dead socket was never reported as dead");
}

/// Following a channel means changing subscriptions on a live socket. Sent as
/// one frame per twenty topics, because Twitch closes a connection carrying
/// more than that at once.
#[tokio::test]
async fn subscriptions_can_be_changed_on_a_live_socket() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::Normal).await;

    let conn = pubsub::connect(&url, "token", vec![Topic::UserDrops.as_str("1")])
        .await
        .unwrap();

    let topics: Vec<String> = (0..45)
        .map(|i| Topic::StreamState.as_str(&i.to_string()))
        .collect();
    assert!(conn.listen(topics).await);

    // Give the frames time to reach the server.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let seen = server.topics_seen();
    let added: Vec<&String> = seen
        .iter()
        .filter(|t| t.starts_with("video-playback-by-id."))
        .collect();
    assert_eq!(added.len(), 45, "topics were lost: {seen:?}");

    for frame in server.listens() {
        let count = frame["data"]["topics"].as_array().map_or(0, |a| a.len());
        assert!(
            count <= pubsub::MAX_TOPICS_PER_FRAME,
            "a frame carried {count} topics; Twitch closes the socket for that"
        );
    }
}

/// Unsubscribing has to reach the server, or a channel we stopped watching
/// keeps delivering events we then have to ignore.
#[tokio::test]
async fn unsubscribing_reaches_the_server() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::Normal).await;

    let conn = pubsub::connect(&url, "token", vec![Topic::UserDrops.as_str("1")])
        .await
        .unwrap();
    assert!(conn.unlisten(vec![Topic::StreamState.as_str("99")]).await);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let unlistens: Vec<Value> = server
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter(|f| f.get("type").and_then(Value::as_str) == Some("UNLISTEN"))
        .cloned()
        .collect();
    assert_eq!(unlistens.len(), 1, "the unsubscribe never arrived");
    assert_eq!(unlistens[0]["data"]["topics"][0], "video-playback-by-id.99");
}

/// Every subscription frame must carry the token, or Twitch rejects it and the
/// topic silently delivers nothing.
#[tokio::test]
async fn every_subscription_frame_carries_the_token() {
    let server = Arc::new(Server::default());
    let url = fake_pubsub(Arc::clone(&server), Mode::Normal).await;

    let conn = pubsub::connect(&url, "the-token", vec![Topic::UserDrops.as_str("1")])
        .await
        .unwrap();
    assert!(conn.listen(vec![Topic::StreamState.as_str("7")]).await);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for frame in server.listens() {
        assert_eq!(
            frame["data"]["auth_token"], "the-token",
            "a frame went out unauthenticated: {frame}"
        );
    }
}
