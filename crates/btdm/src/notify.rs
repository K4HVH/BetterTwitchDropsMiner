//! Outbound alerting.
//!
//! Deliberately selective: an unattended miner that pings on every heartbeat
//! trains you to ignore it, so only events you would act on are sent.
//!
//! More than one destination, because "Discord or nothing" is a poor answer
//! for a box you are not sitting at — ntfy in particular needs no account and
//! reaches a phone, which is exactly the shape of "your token died at 3am".

use btdm_core::event::{EventKind, MinerEvent, Severity};
use serde::{Deserialize, Serialize};

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
        // Recovery giving up is the single most important thing to hear about:
        // it is the moment the miner stopped earning and could not fix itself.
        // Saying nothing here is what makes a silent stall silent.
        EventKind::Quarantined { what, until } => Some(format!(
            "`{account}` gave up on **{what}** after repeated failure to earn anything. \
             It will be retried at {}.",
            until.format("%H:%M UTC")
        )),
        EventKind::Notice {
            message,
            severity: Severity::Warn | Severity::Bad,
        } => Some(format!("`{account}`: {message}")),
        _ => None,
    }
}

/// How urgent a notification is, for services that support it.
fn priority_of(event: &MinerEvent) -> Severity {
    event.severity()
}

/// Where alerts go.
///
/// Several may be configured at once; a phone push and a chat message are not
/// the same audience.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifiers {
    /// A Discord webhook URL.
    pub discord: Option<String>,
    /// An ntfy topic URL, e.g. `https://ntfy.sh/my-secret-topic`. Needs no
    /// account and reaches a phone.
    pub ntfy: Option<String>,
    /// Any URL that accepts a JSON POST. The body carries the account, the
    /// text, the severity and the raw event.
    pub webhook: Option<String>,
}

impl Notifiers {
    pub fn is_empty(&self) -> bool {
        self.discord.is_none() && self.ntfy.is_none() && self.webhook.is_none()
    }

    /// Merges a per-account override over the installation-wide defaults.
    pub fn merged_with(&self, over: &Notifiers) -> Notifiers {
        Notifiers {
            discord: over.discord.clone().or_else(|| self.discord.clone()),
            ntfy: over.ntfy.clone().or_else(|| self.ntfy.clone()),
            webhook: over.webhook.clone().or_else(|| self.webhook.clone()),
        }
    }
}

pub struct Notifier {
    http: reqwest::Client,
    targets: Notifiers,
}

impl Notifier {
    pub fn new(http: reqwest::Client, targets: Notifiers) -> Self {
        Self { http, targets }
    }

    pub async fn notify(&self, account: &str, event: &MinerEvent) {
        let Some(text) = describe(account, event) else {
            return;
        };
        self.send(account, &text, event).await;
    }

    /// Posts to every configured destination.
    ///
    /// A failed notification must never take the miner down, and one
    /// destination being unreachable must not stop the others: they are
    /// different audiences, and the point of configuring two is that one of
    /// them works.
    pub async fn send(&self, account: &str, text: &str, event: &MinerEvent) {
        let severity = priority_of(event);

        if let Some(url) = &self.targets.discord {
            let body = serde_json::json!({ "content": text });
            self.post(url, &body, "discord").await;
        }

        if let Some(url) = &self.targets.ntfy {
            // ntfy takes the message as the raw body, with metadata in headers.
            let priority = match severity {
                Severity::Bad => "urgent",
                Severity::Warn => "high",
                Severity::Good => "default",
                _ => "low",
            };
            let tag = match severity {
                Severity::Bad => "rotating_light",
                Severity::Warn => "warning",
                Severity::Good => "tada",
                _ => "information_source",
            };
            let result = self
                .http
                .post(url)
                .header("Title", format!("btdm: {account}"))
                .header("Priority", priority)
                .header("Tags", tag)
                // Markdown emphasis is Discord's; ntfy shows it literally.
                .body(text.replace("**", "").replace('`', ""))
                .send()
                .await;
            self.report(result, "ntfy").await;
        }

        if let Some(url) = &self.targets.webhook {
            let body = serde_json::json!({
                "account": account,
                "text": text,
                "severity": severity,
                "at": event.at,
                "event": event.kind,
            });
            self.post(url, &body, "webhook").await;
        }
    }

    async fn post(&self, url: &str, body: &serde_json::Value, what: &str) {
        let result = self.http.post(url).json(body).send().await;
        self.report(result, what).await;
    }

    async fn report(&self, result: Result<reqwest::Response, reqwest::Error>, what: &str) {
        match result {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!(target_kind = what, status = %r.status(), "notification rejected"),
            Err(e) => tracing::warn!(target_kind = what, error = %e, "notification target unreachable"),
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
            EventKind::StoppedWatching {
                channel: ChannelId::from("c1"),
                reason: btdm_core::event::StopReason::BetterChannel,
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

    #[test]
    fn no_destinations_is_recognised_as_nothing_to_do() {
        assert!(Notifiers::default().is_empty());
        assert!(
            !Notifiers {
                ntfy: Some("https://ntfy.sh/x".into()),
                ..Default::default()
            }
            .is_empty()
        );
    }

    /// An account with its own destination must not also get the shared one
    /// for that channel — two copies of every alert is how people mute them.
    #[test]
    fn a_per_account_destination_overrides_the_shared_one() {
        let global = Notifiers {
            discord: Some("https://global/discord".into()),
            ntfy: Some("https://global/ntfy".into()),
            webhook: None,
        };
        let account = Notifiers {
            discord: Some("https://mine/discord".into()),
            ..Default::default()
        };

        let merged = global.merged_with(&account);
        assert_eq!(merged.discord.as_deref(), Some("https://mine/discord"));
        assert_eq!(
            merged.ntfy.as_deref(),
            Some("https://global/ntfy"),
            "what the account did not override still applies"
        );
    }
}

#[cfg(test)]
mod stall_alert_tests {
    use super::*;
    use btdm_core::event::Severity;

    fn event(kind: EventKind) -> MinerEvent {
        MinerEvent {
            at: chrono::Utc::now(),
            kind,
        }
    }

    /// The whole point of alerting is to hear when mining stopped earning and
    /// could not recover on its own. Staying quiet here is the failure mode
    /// this project exists to fix.
    #[test]
    fn giving_up_on_a_campaign_raises_an_alert() {
        let text = describe(
            "main",
            &event(EventKind::Quarantined {
                what: "Deadlock Twitch Drops".into(),
                until: chrono::Utc::now() + chrono::Duration::hours(2),
            }),
        )
        .expect("a silent stall must not be silent");
        assert!(text.contains("Deadlock Twitch Drops"), "{text}");
        assert!(text.contains("gave up"), "{text}");
    }

    #[test]
    fn a_warning_notice_is_forwarded_but_an_informational_one_is_not() {
        assert!(
            describe(
                "main",
                &event(EventKind::Notice {
                    message: "spade.twitch.tv is unreachable".into(),
                    severity: Severity::Bad,
                })
            )
            .is_some()
        );
        assert!(
            describe(
                "main",
                &event(EventKind::Notice {
                    message: "checked 40 channels".into(),
                    severity: Severity::Info,
                })
            )
            .is_none(),
            "routine notices must not ping the user"
        );
    }
}

#[cfg(test)]
mod delivery_tests {
    //! Alerts actually arriving, against real sockets.
    //!
    //! The tests above decide *whether* an event is worth reporting. These
    //! prove the message reaches every configured destination, in the shape
    //! that destination expects, and that one being down does not silence the
    //! others — which is the entire reason for configuring two.
    use super::*;
    use btdm_core::model::DropId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    async fn sink(status: u16) -> (String, Seen, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (s, h) = (Arc::clone(&seen), Arc::clone(&hits));

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                h.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 65536];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                s.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = socket
                    .write_all(
                        format!("HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                            .as_bytes(),
                    )
                    .await;
            }
        });

        (format!("http://{addr}/hook"), seen, hits)
    }

    fn claimed() -> MinerEvent {
        MinerEvent {
            at: chrono::Utc::now(),
            kind: EventKind::DropClaimed {
                drop: DropId::from("d1"),
                name: "Golden Skin".into(),
            },
        }
    }

    #[tokio::test]
    async fn every_configured_destination_receives_the_alert() {
        let (discord, discord_seen, _) = sink(204).await;
        let (ntfy, ntfy_seen, _) = sink(200).await;
        let (webhook, webhook_seen, _) = sink(200).await;

        Notifier::new(
            reqwest::Client::new(),
            Notifiers {
                discord: Some(discord),
                ntfy: Some(ntfy),
                webhook: Some(webhook),
            },
        )
        .notify("main", &claimed())
        .await;

        let d = discord_seen.lock().unwrap().join("");
        assert!(d.contains("Golden Skin"), "discord did not get it: {d}");
        assert!(d.contains("\"content\""), "wrong shape for discord");

        // Header names arrive lowercased on the wire.
        let t = ntfy_seen.lock().unwrap().join("").to_lowercase();
        assert!(t.contains("golden skin"), "ntfy did not get it");
        assert!(t.contains("title: btdm: main"), "ntfy needs its title header");
        assert!(t.contains("priority:"), "ntfy needs a priority");
        assert!(!t.contains("**"), "ntfy shows markdown emphasis literally");

        let w = webhook_seen.lock().unwrap().join("");
        assert!(w.contains("Golden Skin"));
        assert!(w.contains("\"severity\""), "a generic webhook needs the severity");
        assert!(w.contains("drop_claimed"), "and the structured event");
    }

    /// The point of configuring two destinations is that one of them works.
    #[tokio::test]
    async fn one_destination_being_down_does_not_silence_the_others() {
        let (good, good_seen, _) = sink(200).await;

        Notifier::new(
            reqwest::Client::new(),
            Notifiers {
                // Nothing is listening here.
                discord: Some("http://127.0.0.1:1/nope".into()),
                ntfy: Some(good),
                webhook: None,
            },
        )
        .notify("main", &claimed())
        .await;

        assert!(
            good_seen.lock().unwrap().join("").contains("Golden Skin"),
            "a dead destination silenced a live one"
        );
    }

    /// A rejection is logged, never retried into a loop, and never fatal.
    #[tokio::test]
    async fn a_rejecting_destination_is_survivable() {
        let (bad, _, hits) = sink(500).await;
        Notifier::new(
            reqwest::Client::new(),
            Notifiers {
                discord: Some(bad),
                ..Default::default()
            },
        )
        .notify("main", &claimed())
        .await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "alerts are not retried");
    }

    #[tokio::test]
    async fn a_routine_event_reaches_nobody() {
        let (url, _, hits) = sink(200).await;
        Notifier::new(
            reqwest::Client::new(),
            Notifiers {
                discord: Some(url),
                ..Default::default()
            },
        )
        .notify(
            "main",
            &MinerEvent {
                at: chrono::Utc::now(),
                kind: EventKind::WatchSent {
                    channel: btdm_core::model::ChannelId::from("c1"),
                },
            },
        )
        .await;

        assert_eq!(hits.load(Ordering::SeqCst), 0, "a heartbeat woke somebody up");
    }

    /// A stall alert must be marked urgent, or it arrives silently at 3am and
    /// nobody sees it until morning.
    #[tokio::test]
    async fn a_stall_is_sent_at_a_higher_priority_than_a_claim() {
        let (ntfy, seen, _) = sink(200).await;
        let n = Notifier::new(
            reqwest::Client::new(),
            Notifiers {
                ntfy: Some(ntfy),
                ..Default::default()
            },
        );

        n.notify(
            "main",
            &MinerEvent {
                at: chrono::Utc::now(),
                kind: EventKind::Quarantined {
                    what: "Some Campaign".into(),
                    until: chrono::Utc::now(),
                },
            },
        )
        .await;

        let sent = seen.lock().unwrap().join("").to_lowercase();
        assert!(
            sent.contains("priority: high") || sent.contains("priority: urgent"),
            "a stall must not arrive silently: {sent}"
        );
    }
}
