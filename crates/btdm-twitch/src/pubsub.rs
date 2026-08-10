//! Twitch PubSub.
//!
//! Pushes drop progress, claim availability and stream up/down. The engine
//! also polls as a backstop, because a socket that is connected but silent
//! looks identical to one that is merely quiet — so this client additionally
//! proves liveness by requiring a PONG within a deadline after each PING.

use crate::TwitchError;
use btdm_core::api::DropProgress;
use btdm_core::model::{ChannelId, DropId};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

pub const PUBSUB_URL: &str = "wss://pubsub-edge.twitch.tv/v1";
/// Twitch drops the connection if it sees no PING within ~5 minutes.
pub const PING_INTERVAL: Duration = Duration::from_secs(180);
/// A PONG must arrive within this window or the socket is considered dead.
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for Twitch to acknowledge a LISTEN.
pub const LISTEN_ACK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    UserDrops,
    UserPoints,
    StreamState,
}

impl Topic {
    pub fn as_str(&self, target_id: &str) -> String {
        let prefix = match self {
            Topic::UserDrops => "user-drop-events",
            Topic::UserPoints => "community-points-user-v1",
            Topic::StreamState => "video-playback-by-id",
        };
        format!("{prefix}.{target_id}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubMessage {
    DropProgress(DropProgress),
    DropClaimable {
        drop_id: DropId,
        claim_id: String,
    },
    StreamUp {
        channel_id: ChannelId,
    },
    StreamDown {
        channel_id: ChannelId,
    },
    PointsClaimable {
        channel_id: ChannelId,
        claim_id: String,
    },
    /// The socket died. The supervisor reconnects; the engine treats this as a
    /// reason to distrust push data until it is back.
    Disconnected,
}

/// PubSub wraps a JSON *string* inside a JSON envelope, so this decodes twice.
pub fn parse_pubsub_frame(raw: &str) -> Option<PubSubMessage> {
    let envelope: Value = serde_json::from_str(raw).ok()?;
    if envelope.get("type")?.as_str()? != "MESSAGE" {
        return None;
    }
    let data = envelope.get("data")?;
    let topic = data.get("topic")?.as_str()?;
    let inner: Value = serde_json::from_str(data.get("message")?.as_str()?).ok()?;
    let target_id = topic.split_once('.').map(|(_, id)| id).unwrap_or_default();

    match inner.get("type").and_then(Value::as_str)? {
        "drop-progress" => {
            let d = inner.get("data")?;
            Some(PubSubMessage::DropProgress(DropProgress {
                drop_id: DropId(d.get("drop_id")?.as_str()?.to_owned()),
                current_minutes: d.get("current_progress_min")?.as_u64()? as u32,
                required_minutes: d.get("required_progress_min")?.as_u64()? as u32,
            }))
        }
        "drop-claim" => {
            let d = inner.get("data")?;
            Some(PubSubMessage::DropClaimable {
                drop_id: DropId(d.get("drop_id")?.as_str()?.to_owned()),
                claim_id: d.get("drop_instance_id")?.as_str()?.to_owned(),
            })
        }
        "stream-up" => Some(PubSubMessage::StreamUp {
            channel_id: ChannelId(target_id.to_owned()),
        }),
        "stream-down" => Some(PubSubMessage::StreamDown {
            channel_id: ChannelId(target_id.to_owned()),
        }),
        "points-earned" | "claim-available" => {
            let claim = inner.get("data")?.get("claim")?;
            Some(PubSubMessage::PointsClaimable {
                channel_id: ChannelId(claim.get("channel_id")?.as_str()?.to_owned()),
                claim_id: claim.get("id")?.as_str()?.to_owned(),
            })
        }
        _ => None,
    }
}

/// A live PubSub connection: decoded messages, plus a way to change what is
/// subscribed without reconnecting.
pub struct Connection {
    pub messages: mpsc::Receiver<PubSubMessage>,
    control: mpsc::Sender<ControlFrame>,
}

enum ControlFrame {
    Listen(Vec<String>),
    Unlisten(Vec<String>),
}

impl Connection {
    /// Subscribes to more topics on the existing socket.
    ///
    /// This exists so the miner can follow the channel it is *currently*
    /// watching. Without it, `video-playback-by-id` was defined, parsed and
    /// handled but never actually subscribed, so the miner learned a stream
    /// had ended only from the watchdog minutes later — heartbeating a dead
    /// broadcast the whole time.
    pub async fn listen(&self, topics: Vec<String>) -> bool {
        !topics.is_empty() && self.control.send(ControlFrame::Listen(topics)).await.is_ok()
    }

    pub async fn unlisten(&self, topics: Vec<String>) -> bool {
        !topics.is_empty() && self.control.send(ControlFrame::Unlisten(topics)).await.is_ok()
    }

    pub async fn recv(&mut self) -> Option<PubSubMessage> {
        self.messages.recv().await
    }
}

/// Connects, subscribes, and streams decoded messages until the socket dies.
///
/// Emits [`PubSubMessage::Disconnected`] on exit so the caller always learns
/// the push channel is gone rather than simply never hearing from it again.
pub async fn connect(
    url: &str,
    token: &str,
    topics: Vec<String>,
) -> Result<Connection, TwitchError> {
    use tokio_tungstenite::tungstenite::Message;

    let (stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| TwitchError::Network(e.to_string()))?;
    let (mut write, mut read) = stream.split();

    let listen = json!({
        "type": "LISTEN",
        "nonce": "btdm",
        "data": { "topics": topics, "auth_token": token },
    });
    write
        .send(Message::Text(listen.to_string().into()))
        .await
        .map_err(|e| TwitchError::Network(e.to_string()))?;

    // Wait for the subscription to be acknowledged. Without this a rejected
    // LISTEN (bad auth, bad topic) leaves a socket that answers PING forever
    // and delivers nothing — indistinguishable from a healthy one.
    let ack = tokio::time::timeout(LISTEN_ACK_TIMEOUT, async {
        while let Some(frame) = read.next().await {
            let Ok(Message::Text(text)) = frame else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) == Some("RESPONSE") {
                return match v.get("error").and_then(Value::as_str) {
                    None | Some("") => Ok(()),
                    Some(e) => Err(TwitchError::Malformed(format!(
                        "pubsub rejected the subscription: {e}"
                    ))),
                };
            }
        }
        Err(TwitchError::Network(
            "pubsub closed before acknowledging the subscription".into(),
        ))
    })
    .await;

    match ack {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(TwitchError::Network(
                "pubsub did not acknowledge the subscription in time".into(),
            ));
        }
    }

    let (tx, rx) = mpsc::channel(64);
    let (control_tx, mut control_rx) = mpsc::channel::<ControlFrame>(16);
    let auth = token.to_owned();

    tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        // The first tick fires immediately; we just connected, so skip it.
        ping.tick().await;
        // When set, a PONG is overdue and the socket is presumed dead.
        let mut pong_deadline: Option<tokio::time::Instant> = None;

        loop {
            let deadline_sleep = async {
                match pong_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                change = control_rx.recv() => {
                    let Some(change) = change else { break };
                    let (kind, topics) = match change {
                        ControlFrame::Listen(t) => ("LISTEN", t),
                        ControlFrame::Unlisten(t) => ("UNLISTEN", t),
                    };
                    let frame = json!({
                        "type": kind,
                        "nonce": "btdm-live",
                        "data": { "topics": topics, "auth_token": auth },
                    });
                    if write.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                    tracing::debug!(kind, count = topics.len(), "pubsub subscription changed");
                }
                _ = ping.tick() => {
                    if write.send(Message::Text(r#"{"type":"PING"}"#.into())).await.is_err() {
                        break;
                    }
                    pong_deadline = Some(tokio::time::Instant::now() + PONG_TIMEOUT);
                }
                _ = deadline_sleep => {
                    tracing::warn!("pubsub did not answer PING; treating the socket as dead");
                    break;
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if text.contains("\"PONG\"") {
                                pong_deadline = None;
                                continue;
                            }
                            if let Some(msg) = parse_pubsub_frame(&text)
                                && tx.send(msg).await.is_err()
                            {
                                break;
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "pubsub socket error");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        tracing::info!("pubsub connection closed");
        let _ = tx.send(PubSubMessage::Disconnected).await;
    });

    Ok(Connection {
        messages: rx,
        control: control_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(topic: &str, inner: Value) -> String {
        json!({
            "type": "MESSAGE",
            "data": { "topic": topic, "message": inner.to_string() }
        })
        .to_string()
    }

    #[test]
    fn topics_render_with_their_target_id() {
        assert_eq!(Topic::UserDrops.as_str("999"), "user-drop-events.999");
        assert_eq!(Topic::StreamState.as_str("12345"), "video-playback-by-id.12345");
        assert_eq!(
            Topic::UserPoints.as_str("999"),
            "community-points-user-v1.999"
        );
    }

    #[test]
    fn parses_a_drop_progress_message() {
        let raw = frame(
            "user-drop-events.999",
            json!({
                "type": "drop-progress",
                "data": { "drop_id": "d1", "current_progress_min": 23, "required_progress_min": 60 }
            }),
        );
        match parse_pubsub_frame(&raw) {
            Some(PubSubMessage::DropProgress(p)) => {
                assert_eq!(p.drop_id.0, "d1");
                assert_eq!(p.current_minutes, 23);
                assert_eq!(p.required_minutes, 60);
            }
            other => panic!("expected drop progress, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_claimable_drop_message() {
        let raw = frame(
            "user-drop-events.999",
            json!({
                "type": "drop-claim",
                "data": { "drop_id": "d1", "drop_instance_id": "inst-9" }
            }),
        );
        match parse_pubsub_frame(&raw) {
            Some(PubSubMessage::DropClaimable { drop_id, claim_id }) => {
                assert_eq!(drop_id.0, "d1");
                assert_eq!(claim_id, "inst-9");
            }
            other => panic!("expected a claimable drop, got {other:?}"),
        }
    }

    #[test]
    fn parses_stream_up_and_down() {
        let up = frame("video-playback-by-id.12345", json!({ "type": "stream-up" }));
        assert_eq!(
            parse_pubsub_frame(&up),
            Some(PubSubMessage::StreamUp {
                channel_id: ChannelId::from("12345")
            })
        );

        let down = frame("video-playback-by-id.12345", json!({ "type": "stream-down" }));
        assert_eq!(
            parse_pubsub_frame(&down),
            Some(PubSubMessage::StreamDown {
                channel_id: ChannelId::from("12345")
            })
        );
    }

    #[test]
    fn parses_a_points_claim() {
        let raw = frame(
            "community-points-user-v1.999",
            json!({
                "type": "claim-available",
                "data": { "claim": { "id": "claim-1", "channel_id": "12345" } }
            }),
        );
        match parse_pubsub_frame(&raw) {
            Some(PubSubMessage::PointsClaimable {
                channel_id,
                claim_id,
            }) => {
                assert_eq!(channel_id.0, "12345");
                assert_eq!(claim_id, "claim-1");
            }
            other => panic!("expected a points claim, got {other:?}"),
        }
    }

    #[test]
    fn ignores_pong_response_and_junk_frames() {
        assert!(parse_pubsub_frame(r#"{"type":"PONG"}"#).is_none());
        assert!(parse_pubsub_frame(r#"{"type":"RESPONSE","error":""}"#).is_none());
        assert!(parse_pubsub_frame("not json at all").is_none());
        assert!(parse_pubsub_frame(r#"{"type":"MESSAGE"}"#).is_none());
    }

    #[test]
    fn an_unknown_message_type_is_ignored_not_fatal() {
        let raw = frame("user-drop-events.999", json!({ "type": "some-new-thing" }));
        assert!(parse_pubsub_frame(&raw).is_none());
    }
}

#[cfg(test)]
mod subscription_tests {
    use super::*;

    /// The stream-state topic was defined, parsed and handled — but never
    /// subscribed, so the whole path was unreachable and the miner kept
    /// heartbeating streams that had already ended.
    #[test]
    fn the_stream_state_topic_is_addressable_per_channel() {
        let topic = Topic::StreamState.as_str("12345");
        assert_eq!(topic, "video-playback-by-id.12345");
    }

    #[tokio::test]
    async fn a_subscription_change_is_queued_for_the_socket() {
        let (tx, mut rx) = mpsc::channel(4);
        let (_msg_tx, msg_rx) = mpsc::channel(1);
        let conn = Connection {
            messages: msg_rx,
            control: tx,
        };

        assert!(conn.listen(vec!["video-playback-by-id.1".into()]).await);
        assert!(matches!(rx.recv().await, Some(ControlFrame::Listen(t)) if t.len() == 1));

        assert!(conn.unlisten(vec!["video-playback-by-id.1".into()]).await);
        assert!(matches!(rx.recv().await, Some(ControlFrame::Unlisten(t)) if t.len() == 1));
    }

    /// Asking for nothing must not put an empty frame on the wire.
    #[tokio::test]
    async fn an_empty_change_is_a_no_op() {
        let (tx, mut rx) = mpsc::channel(4);
        let (_msg_tx, msg_rx) = mpsc::channel(1);
        let conn = Connection {
            messages: msg_rx,
            control: tx,
        };

        assert!(!conn.listen(vec![]).await);
        assert!(rx.try_recv().is_err());
    }

    /// A dead socket must report failure rather than silently swallowing the
    /// change and leaving the miner blind to stream endings.
    #[tokio::test]
    async fn a_closed_socket_reports_the_failure() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let (_msg_tx, msg_rx) = mpsc::channel(1);
        let conn = Connection {
            messages: msg_rx,
            control: tx,
        };
        assert!(!conn.listen(vec!["video-playback-by-id.1".into()]).await);
    }
}
