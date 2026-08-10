//! Outbound alerting.
//!
//! Deliberately selective: an unattended miner that pings on every heartbeat
//! trains you to ignore it, so only events you would act on are sent.

use btdm_core::event::{EventKind, MinerEvent};

/// Whether an event is worth interrupting someone for, and how to phrase it.
///
/// Routine events (heartbeats, campaign syncs, individual rejections) are
/// deliberately absent.
pub fn describe(account: &str, event: &MinerEvent) -> Option<String> {
    match &event.kind {
        EventKind::DropClaimed { name, .. } => {
            Some(format!("`{account}` claimed a drop: **{name}**"))
        }
        EventKind::AuthExpired => Some(format!(
            "`{account}` needs re-authorising — mining has stopped. Run `btdm login --force`."
        )),
        EventKind::AuthSucceeded { login, .. } => {
            Some(format!("`{account}` linked to Twitch user `{login}`"))
        }
        EventKind::AuthCodeIssued {
            user_code,
            verification_uri,
        } => Some(format!(
            "`{account}` is waiting for authorisation: enter `{user_code}` at {verification_uri}"
        )),
        _ => None,
    }
}

pub struct Discord {
    http: reqwest::Client,
    webhook: String,
}

impl Discord {
    pub fn new(http: reqwest::Client, webhook: String) -> Self {
        Self { http, webhook }
    }

    /// Posts a message. A failed notification must never take the miner down,
    /// so errors are logged and swallowed.
    pub async fn send(&self, content: &str) {
        let body = serde_json::json!({ "content": content });
        match self.http.post(&self.webhook).json(&body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!(status = %r.status(), "discord webhook rejected the message"),
            Err(e) => tracing::warn!(error = %e, "discord webhook unreachable"),
        }
    }

    pub async fn notify(&self, account: &str, event: &MinerEvent) {
        if let Some(text) = describe(account, event) {
            self.send(&text).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btdm_core::model::{ChannelId, DropId, UserId};

    fn event(kind: EventKind) -> MinerEvent {
        MinerEvent {
            at: chrono::Utc::now(),
            kind,
        }
    }

    #[test]
    fn a_claimed_drop_is_worth_reporting() {
        let text = describe(
            "main",
            &event(EventKind::DropClaimed {
                drop: DropId::from("d1"),
                name: "Shiny Skin".into(),
            }),
        )
        .expect("claiming is newsworthy");
        assert!(text.contains("Shiny Skin"));
        assert!(text.contains("main"));
    }

    #[test]
    fn expired_auth_says_what_to_do_about_it() {
        let text = describe("main", &event(EventKind::AuthExpired)).unwrap();
        assert!(
            text.contains("btdm login"),
            "an alert without a next step is just noise"
        );
    }

    #[test]
    fn a_pending_login_code_is_relayed() {
        let text = describe(
            "main",
            &event(EventKind::AuthCodeIssued {
                user_code: "ABCDEFGH".into(),
                verification_uri: "https://twitch.tv/activate".into(),
            }),
        )
        .unwrap();
        assert!(text.contains("ABCDEFGH"));
    }

    /// Routine events must stay silent, or the alerts become noise and get
    /// muted — at which point the important ones are missed too.
    #[test]
    fn routine_events_are_not_reported() {
        for kind in [
            EventKind::WatchSent {
                channel: ChannelId::from("c1"),
            },
            EventKind::CampaignsSynced {
                total: 61,
                eligible: 7,
            },
            EventKind::ProgressConfirmed {
                drop: DropId::from("d1"),
                current_minutes: 5,
                required_minutes: 60,
            },
            EventKind::ChannelOffline {
                channel: ChannelId::from("c1"),
            },
            EventKind::PointsClaimed {
                channel: ChannelId::from("c1"),
            },
        ] {
            assert!(
                describe("main", &event(kind.clone())).is_none(),
                "{kind:?} should not raise an alert"
            );
        }
    }

    #[test]
    fn a_successful_login_is_reported() {
        assert!(
            describe(
                "main",
                &event(EventKind::AuthSucceeded {
                    user_id: UserId::from("1"),
                    login: "someone".into(),
                })
            )
            .is_some()
        );
    }
}
