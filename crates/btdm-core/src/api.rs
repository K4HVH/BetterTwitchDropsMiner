//! The seam between the mining engine and Twitch.
//!
//! The engine depends only on this trait, so tests can substitute a fake and
//! script failure scenarios that are impossible to trigger against the real API.

use crate::model::*;

#[derive(Debug, thiserror::Error)]
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

#[allow(async_fn_in_trait)]
pub trait TwitchApi: Send + Sync {
    /// Every campaign Twitch offers this account, whether or not it has been
    /// started. Without this the miner could only ever continue campaigns it
    /// had already begun.
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError>;

    /// Campaigns with in-progress or claimable drops for this account.
    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError>;

    /// Full detail for one campaign: drop tiers, allow-list and link status.
    async fn campaign_details(&self, id: &CampaignId) -> Result<Option<DropCampaign>, ApiError>;

    /// Which drops this channel can actually progress right now.
    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError>;

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

    async fn claim_drop(&self, claim_id: &str) -> Result<(), ApiError>;

    /// Claims the channel-points bonus chest on a channel.
    async fn claim_points(&self, channel: &ChannelId, claim_id: &str) -> Result<(), ApiError>;
}
