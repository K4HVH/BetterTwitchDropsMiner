//! Everything the miner does is published as an event. The UI, the notifier
//! and the log are all consumers of this one stream, so a headless daemon and
//! an interactive session cannot diverge in behaviour.
//!
//! Each event knows how to describe itself. The dashboard used to render
//! `format!("{:?}", kind)`, which showed users lines like
//! `CampaignRejected { campaign: CampaignId("1234"), reason: NotLinked }`.
//! That is a debug representation leaking into a user interface, so the text
//! and the severity now live here where they can be tested.

use crate::model::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinerEvent {
    pub at: DateTime<Utc>,
    pub kind: EventKind,
}

impl MinerEvent {
    pub fn summary(&self) -> String {
        self.kind.summary()
    }

    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

/// How much attention an event deserves. Drives colour in the dashboard and
/// whether the Discord notifier bothers the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Routine bookkeeping.
    Debug,
    Info,
    /// Something went right and is worth celebrating.
    Good,
    /// Something is wrong but recovery is under way.
    Warn,
    /// Mining has stopped or needs the user.
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Login required; show this code to the user.
    AuthCodeIssued {
        user_code: String,
        verification_uri: String,
    },
    AuthSucceeded {
        user_id: UserId,
        login: String,
    },
    AuthExpired,
    CampaignsSynced {
        total: usize,
        eligible: usize,
        /// Campaigns Twitch offered whose detail could not be fetched. They
        /// are neither mineable nor rejected — nobody knows which, and the
        /// count exists so the two numbers either side of it stop implying
        /// that they do. Defaulted so events written before it existed still
        /// load.
        #[serde(default)]
        unchecked: usize,
    },
    /// A campaign was rejected, with the reason, so the UI never has to guess.
    CampaignRejected {
        campaign: CampaignId,
        reason: RejectReason,
    },
    NowWatching {
        channel: Channel,
        campaign: CampaignId,
        /// Every campaign this channel advances at once.
        campaigns: usize,
        /// Why the strategy chose it.
        reason: String,
    },
    WatchSent {
        channel: ChannelId,
    },
    WatchFailed {
        channel: ChannelId,
        error: String,
    },
    /// Server-confirmed progress. The only source the UI is allowed to display.
    ProgressConfirmed {
        drop: DropId,
        current_minutes: u32,
        required_minutes: u32,
    },
    DropClaimed {
        drop: DropId,
        name: String,
    },
    /// Left a channel, with the reason. "Offline" was previously reported for
    /// every departure including deliberate switches, which made the log lie.
    StoppedWatching {
        channel: ChannelId,
        reason: StopReason,
    },
    PointsClaimed {
        channel: ChannelId,
    },
    /// A recovery rung was worked.
    Recovering {
        step: String,
        detail: String,
    },
    /// A channel or campaign was rested after repeated failure.
    Quarantined {
        what: String,
        until: DateTime<Utc>,
    },
    /// A user command took effect.
    CommandApplied {
        message: String,
    },
    Paused,
    Resumed,
    /// Anything worth telling the user that is not one of the above.
    Notice {
        message: String,
        severity: Severity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Twitch reported the stream ended.
    WentOffline,
    /// The strategy chose somewhere better.
    BetterChannel,
    /// Recovery gave up on this channel.
    NotEarning,
    /// The user asked.
    UserRequest,
    /// An API error made the session untrustworthy.
    Error,
}

impl StopReason {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::WentOffline => "the stream ended",
            Self::BetterChannel => "a better channel was available",
            Self::NotEarning => "it was earning nothing",
            Self::UserRequest => "you asked",
            Self::Error => "an error made the session untrustworthy",
        }
    }
}

impl EventKind {
    /// One line a person can read, with no Rust syntax in it.
    pub fn summary(&self) -> String {
        match self {
            Self::AuthCodeIssued {
                user_code,
                verification_uri,
            } => format!("Enter code {user_code} at {verification_uri}"),
            Self::AuthSucceeded { login, .. } => format!("Signed in as {login}"),
            Self::AuthExpired => "The Twitch token expired — sign in again".into(),
            Self::CampaignsSynced {
                total,
                eligible,
                unchecked,
            } => {
                let mut summary = format!("Synced {total} campaigns, {eligible} mineable");
                // Sixteen campaigns once fell out of a sync between those two
                // numbers, and the line still read "Synced 84 campaigns, 3
                // mineable" as though it had covered everything. A campaign
                // nobody could look at is not a campaign that is not worth
                // mining, and the summary must not round it to one.
                if *unchecked > 0 {
                    summary.push_str(&format!(", {unchecked} could not be checked"));
                }
                summary
            }
            Self::CampaignRejected { campaign, reason } => {
                format!("Skipping campaign {campaign}: {}", reason.explain())
            }
            Self::NowWatching {
                channel,
                campaigns,
                reason,
                ..
            } => {
                let scope = if *campaigns > 1 {
                    format!(" ({campaigns} campaigns at once)")
                } else {
                    String::new()
                };
                format!("Watching {}{scope} — {reason}", channel.display_name)
            }
            Self::WatchSent { .. } => "Heartbeat sent".into(),
            Self::WatchFailed { error, .. } => format!("Heartbeat failed: {error}"),
            Self::ProgressConfirmed {
                current_minutes,
                required_minutes,
                ..
            } => format!("Twitch confirmed {current_minutes}/{required_minutes} minutes"),
            Self::DropClaimed { name, .. } => format!("Claimed {name}"),
            Self::StoppedWatching { reason, .. } => {
                format!("Stopped watching — {}", reason.describe())
            }
            Self::PointsClaimed { .. } => "Claimed channel points".into(),
            Self::Recovering { step, detail } => format!("Recovery ({step}): {detail}"),
            Self::Quarantined { what, .. } => format!("Resting {what} after repeated failure"),
            Self::CommandApplied { message } => message.clone(),
            Self::Paused => "Mining paused".into(),
            Self::Resumed => "Mining resumed".into(),
            Self::Notice { message, .. } => message.clone(),
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            Self::DropClaimed { .. } | Self::AuthSucceeded { .. } => Severity::Good,
            Self::AuthExpired | Self::WatchFailed { .. } => Severity::Bad,
            Self::Recovering { .. } | Self::Quarantined { .. } => Severity::Warn,
            Self::StoppedWatching {
                reason: StopReason::NotEarning | StopReason::Error,
                ..
            } => Severity::Warn,
            // Heartbeats and per-campaign rejections happen constantly; they
            // belong in the log but must not colour it.
            Self::WatchSent { .. } | Self::CampaignRejected { .. } => Severity::Debug,
            Self::Notice { severity, .. } => *severity,
            _ => Severity::Info,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The account is not linked to this campaign — mining it can never progress.
    NotLinked,
    Expired,
    NotStarted,
    AllDropsClaimed,
    NoEligibleChannel,
    /// Cannot be finished before its deadline, so mining it wastes the slot.
    Infeasible,
    /// The user excluded it.
    Excluded,
    /// Every unclaimed drop needs a subscription, which watching cannot earn.
    SubscriptionRequired,
}

impl RejectReason {
    /// Plain English. `NotLinked` means nothing to someone who does not
    /// already know it refers to Twitch account linking.
    pub fn explain(&self) -> &'static str {
        match self {
            Self::NotLinked => {
                "your Twitch account is not linked to this campaign's game — \
                 link it on the campaign's website to mine it"
            }
            Self::Expired => "it has already ended",
            Self::NotStarted => "it has not started yet",
            Self::AllDropsClaimed => "every drop in it is already claimed",
            Self::NoEligibleChannel => "nobody is streaming it with drops enabled",
            Self::SubscriptionRequired => {
                "its rewards are for subscribing, not for watching — no amount \
                 of watching will earn them"
            }
            Self::Infeasible => "there is not enough time left to finish it before it ends",
            Self::Excluded => "you excluded it",
        }
    }

    /// A short label for a badge in the UI.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotLinked => "not linked",
            Self::Expired => "ended",
            Self::NotStarted => "not started",
            Self::AllDropsClaimed => "all claimed",
            Self::NoEligibleChannel => "nobody live",
            Self::Infeasible => "not enough time",
            Self::Excluded => "excluded",
            Self::SubscriptionRequired => "needs a sub",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> Channel {
        Channel {
            id: ChannelId::from("1"),
            login: "somestreamer".into(),
            display_name: "SomeStreamer".into(),
        }
    }

    /// The dashboard prints these verbatim, so no Rust debug syntax may leak.
    #[test]
    fn no_summary_contains_rust_debug_syntax() {
        let events = vec![
            EventKind::CampaignsSynced {
                total: 85,
                eligible: 17,
                unchecked: 0,
            },
            EventKind::CampaignRejected {
                campaign: CampaignId::from("c1"),
                reason: RejectReason::NotLinked,
            },
            EventKind::NowWatching {
                channel: channel(),
                campaign: CampaignId::from("c1"),
                campaigns: 3,
                reason: "deadline: soonest deadline has 4h of slack".into(),
            },
            EventKind::DropClaimed {
                drop: DropId::from("d1"),
                name: "Weapon Charm".into(),
            },
            EventKind::StoppedWatching {
                channel: ChannelId::from("1"),
                reason: StopReason::NotEarning,
            },
            EventKind::WatchFailed {
                channel: ChannelId::from("1"),
                error: "429".into(),
            },
        ];

        for e in events {
            let s = e.summary();
            // Braces and `Type(..)` wrappers are the tell-tales of a derived
            // Debug impl. Ordinary prose parentheses are fine.
            for bad in ["{", "}", "Id(", "::", "reason:", "channel:"] {
                assert!(
                    !s.contains(bad),
                    "summary reads like debug output: {s:?} contains {bad:?}"
                );
            }
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn a_multi_campaign_watch_says_so() {
        let s = EventKind::NowWatching {
            channel: channel(),
            campaign: CampaignId::from("c1"),
            campaigns: 3,
            reason: "deadline".into(),
        }
        .summary();
        assert!(s.contains("SomeStreamer"), "{s}");
        assert!(s.contains("3 campaigns"), "{s}");
    }

    #[test]
    fn a_single_campaign_watch_does_not_mention_a_count() {
        let s = EventKind::NowWatching {
            channel: channel(),
            campaign: CampaignId::from("c1"),
            campaigns: 1,
            reason: "deadline".into(),
        }
        .summary();
        assert!(!s.contains("campaigns at once"), "{s}");
    }

    #[test]
    fn claims_and_failures_are_ranked_above_routine_traffic() {
        assert_eq!(
            EventKind::DropClaimed {
                drop: DropId::from("d"),
                name: "x".into()
            }
            .severity(),
            Severity::Good
        );
        assert_eq!(
            EventKind::WatchSent {
                channel: ChannelId::from("1")
            }
            .severity(),
            Severity::Debug
        );
        assert_eq!(EventKind::AuthExpired.severity(), Severity::Bad);
    }

    /// Every reason must have an explanation a non-expert can act on.
    #[test]
    fn every_reject_reason_explains_itself() {
        let all = [
            RejectReason::NotLinked,
            RejectReason::Expired,
            RejectReason::NotStarted,
            RejectReason::AllDropsClaimed,
            RejectReason::NoEligibleChannel,
            RejectReason::Infeasible,
            RejectReason::Excluded,
        ];
        for r in all {
            assert!(r.explain().len() > 10, "{r:?} needs a real explanation");
            assert!(!r.label().is_empty());
            // The label is a badge, so it must be short.
            assert!(r.label().len() <= 16, "{r:?} label is too long for a badge");
        }
    }

    #[test]
    fn events_round_trip_as_json_for_the_dashboard() {
        let e = MinerEvent {
            at: Utc::now(),
            kind: EventKind::CampaignsSynced {
                total: 3,
                eligible: 1,
                unchecked: 0,
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: MinerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, e.kind);
        assert!(json.contains("campaigns_synced"), "{json}");
    }
}
