//! Everything the miner does is published as an event. The UI, the notifier
//! and the log are all consumers of this one stream, so a headless daemon and
//! an interactive session cannot diverge in behaviour.

use crate::model::*;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerEvent {
    pub at: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
    /// A campaign was rejected, with the reason, so the UI never has to guess.
    CampaignRejected {
        campaign: CampaignId,
        reason: RejectReason,
    },
    NowWatching {
        channel: Channel,
        campaign: CampaignId,
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
    ChannelOffline {
        channel: ChannelId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The account is not linked to this campaign — mining it can never progress.
    NotLinked,
    Expired,
    NotStarted,
    AllDropsClaimed,
    NoEligibleChannel,
}
