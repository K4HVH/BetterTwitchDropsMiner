//! The seam between the mining engine and Twitch.
//!
//! The engine depends only on this trait, so tests can substitute a fake and
//! script failure scenarios that are impossible to trigger against the real API.

use crate::model::*;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("network failure: {0}")]
    Network(String),
    #[error("authentication is no longer valid")]
    Unauthorized,
    #[error("rate limited by Twitch")]
    RateLimited,
    #[error("unexpected response shape: {0}")]
    Malformed(String),
}

/// Server-confirmed progress for the drop currently being mined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropProgress {
    pub drop_id: DropId,
    pub current_minutes: u32,
    pub required_minutes: u32,
}

/// One look at the account's inventory.
///
/// The claimed-benefit ledger arrives in the same response as the campaigns
/// and is the only durable record of what the account already owns, so the two
/// travel together rather than costing a second request.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// Campaigns with in-progress or claimable drops for this account.
    pub campaigns: Vec<DropCampaign>,
    /// Benefit id to when Twitch last awarded it. Covers the whole account
    /// history, not just the campaigns above.
    pub awarded: std::collections::HashMap<String, DateTime<Utc>>,
}

impl Inventory {
    /// The campaigns alone, for callers with no use for the ledger.
    pub fn just_campaigns(campaigns: Vec<DropCampaign>) -> Self {
        Self {
            campaigns,
            awarded: std::collections::HashMap::new(),
        }
    }
}

#[allow(async_fn_in_trait)]
pub trait TwitchApi: Send + Sync {
    /// Every campaign Twitch offers this account, whether or not it has been
    /// started. Without this the miner could only ever continue campaigns it
    /// had already begun.
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError>;

    /// Campaigns with in-progress or claimable drops, plus the account's
    /// claimed-benefit ledger.
    async fn fetch_inventory(&self) -> Result<Inventory, ApiError>;

    /// Full detail for one campaign: drop tiers, allow-list and link status.
    async fn campaign_details(&self, id: &CampaignId) -> Result<Option<DropCampaign>, ApiError>;

    /// Details for many campaigns at once. See [`TwitchApi::available_drops_many`].
    async fn campaign_details_many(
        &self,
        ids: &[CampaignId],
    ) -> Vec<(CampaignId, Result<Option<DropCampaign>, ApiError>)> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let result = self.campaign_details(id).await;
            out.push((id.clone(), result));
        }
        out
    }

    /// Which drops this channel can actually progress right now.
    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError>;

    /// The same check for many channels at once.
    ///
    /// A selection pass asks about every live channel it is considering, which
    /// against a real account is well over a hundred. The default answers them
    /// one at a time; a transport that can batch should override this, because
    /// the difference is a hundred and sixty HTTP requests versus eight.
    ///
    /// Results are per channel: one channel failing must not discard the rest,
    /// so a failure is reported in its own slot rather than for the batch.
    async fn available_drops_many(
        &self,
        channels: &[ChannelId],
    ) -> Vec<(ChannelId, Result<Vec<DropId>, ApiError>)> {
        let mut out = Vec::with_capacity(channels.len());
        for channel in channels {
            let result = self.available_drops(channel).await;
            out.push((channel.clone(), result));
        }
        out
    }

    async fn live_channels(
        &self,
        game: &Game,
        limit: u32,
    ) -> Result<Vec<(Channel, Stream)>, ApiError>;

    /// `None` when the channel is offline.
    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError>;

    /// One watch heartbeat. The engine calls this on a fixed interval.
    async fn send_watch(&self, channel: &Channel, stream: &Stream) -> Result<(), ApiError>;

    /// Pull-based progress, the backstop for a live-but-silent websocket.
    async fn current_progress(&self, channel: &ChannelId)
    -> Result<Option<DropProgress>, ApiError>;

    /// Claims an earned drop.
    ///
    /// `claim_id` is Twitch's `dropInstanceID` when it supplied one. It does
    /// not always, so the campaign and drop are passed too: the transport can
    /// reconstruct the id from them, and a drop that is plainly earned is not
    /// abandoned merely because one field was absent.
    async fn claim_drop(
        &self,
        drop: &DropId,
        campaign: &CampaignId,
        claim_id: Option<&str>,
    ) -> Result<(), ApiError>;

    /// Claims the channel-points bonus chest on a channel.
    async fn claim_points(&self, channel: &ChannelId, claim_id: &str) -> Result<(), ApiError>;
}
