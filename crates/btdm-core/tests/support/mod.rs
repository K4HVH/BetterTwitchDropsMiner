//! A scriptable Twitch. Every failure the engine must survive is expressible
//! here, which is the entire reason `btdm-core` is forbidden from doing I/O.

#![allow(dead_code)]

use btdm_core::api::{ApiError, DropProgress, TwitchApi};
use btdm_core::model::*;
use chrono::TimeZone;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeTwitch {
    /// Campaigns already in progress, with full drop detail.
    pub inventory: Mutex<Vec<DropCampaign>>,
    /// Campaigns the dashboard offers, typically without drop detail.
    pub offered: Mutex<Vec<DropCampaign>>,
    /// Full detail keyed by campaign ID, as returned by the details query.
    pub details: Mutex<HashMap<String, DropCampaign>>,
    pub channels: Mutex<Vec<(Channel, Stream)>>,
    /// Drop IDs each channel can progress. A channel absent from this map
    /// progresses nothing, which is how "unminable" is expressed.
    pub available: Mutex<HashMap<String, Vec<DropId>>>,
    pub progress: Mutex<Option<DropProgress>>,
    pub watches_sent: Mutex<Vec<ChannelId>>,
    pub claims: Mutex<Vec<String>>,
    pub point_claims: Mutex<Vec<String>>,
    /// When set, every call fails with this error.
    pub fail_with: Mutex<Option<ApiError>>,
    /// When set, only `available_drops` fails.
    pub fail_available_drops: Mutex<bool>,
    pub detail_fetches: Mutex<usize>,
}

impl FakeTwitch {
    fn guard(&self) -> Result<(), ApiError> {
        match &*self.fail_with.lock().unwrap() {
            Some(ApiError::Unauthorized) => Err(ApiError::Unauthorized),
            Some(ApiError::RateLimited) => Err(ApiError::RateLimited),
            Some(ApiError::Network(m)) => Err(ApiError::Network(m.clone())),
            Some(ApiError::Malformed(m)) => Err(ApiError::Malformed(m.clone())),
            None => Ok(()),
        }
    }
}

impl TwitchApi for FakeTwitch {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        self.guard()?;
        Ok(self.offered.lock().unwrap().clone())
    }

    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError> {
        self.guard()?;
        Ok(self.inventory.lock().unwrap().clone())
    }

    async fn campaign_details(&self, id: &CampaignId) -> Result<Option<DropCampaign>, ApiError> {
        self.guard()?;
        *self.detail_fetches.lock().unwrap() += 1;
        Ok(self.details.lock().unwrap().get(&id.0).cloned())
    }

    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError> {
        self.guard()?;
        if *self.fail_available_drops.lock().unwrap() {
            return Err(ApiError::Network("availability check exploded".into()));
        }
        Ok(self
            .available
            .lock()
            .unwrap()
            .get(&channel.0)
            .cloned()
            .unwrap_or_default())
    }

    async fn live_channels(
        &self,
        game: &Game,
        _limit: u32,
    ) -> Result<Vec<(Channel, Stream)>, ApiError> {
        self.guard()?;
        Ok(self
            .channels
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.game.as_ref().is_some_and(|g| g.id == game.id))
            .cloned()
            .collect())
    }

    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError> {
        self.guard()?;
        Ok(self
            .channels
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c.login == login)
            .map(|(_, s)| s.clone()))
    }

    async fn send_watch(&self, channel: &Channel, _stream: &Stream) -> Result<(), ApiError> {
        self.guard()?;
        self.watches_sent.lock().unwrap().push(channel.id.clone());
        Ok(())
    }

    async fn current_progress(
        &self,
        _channel: &ChannelId,
    ) -> Result<Option<DropProgress>, ApiError> {
        self.guard()?;
        Ok(self.progress.lock().unwrap().clone())
    }

    async fn claim_drop(&self, claim_id: &str) -> Result<(), ApiError> {
        self.guard()?;
        self.claims.lock().unwrap().push(claim_id.to_owned());
        Ok(())
    }

    async fn claim_points(&self, _channel: &ChannelId, claim_id: &str) -> Result<(), ApiError> {
        self.guard()?;
        self.point_claims.lock().unwrap().push(claim_id.to_owned());
        Ok(())
    }
}

pub fn game() -> Game {
    Game {
        id: GameId::from("g1"),
        name: "Deadlock".into(),
        slug: "deadlock".into(),
    }
}

pub fn channel(id: &str, login: &str) -> Channel {
    Channel {
        id: ChannelId::from(id),
        login: login.into(),
        display_name: login.into(),
    }
}

pub fn stream(broadcast: &str, drops_enabled: bool) -> Stream {
    Stream {
        broadcast_id: broadcast.into(),
        game: Some(game()),
        title: "t".into(),
        viewers: 100,
        drops_enabled,
    }
}

pub fn campaign(id: &str, linked: bool, drops: Vec<TimedDrop>) -> DropCampaign {
    DropCampaign {
        id: CampaignId::from(id),
        name: format!("Campaign {id}"),
        game: game(),
        starts_at: chrono::Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
        ends_at: chrono::Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
        drops,
        is_account_linked: linked,
        allowed_channels: vec![],
    }
}

pub fn drop(id: &str, required: u32, current: u32) -> TimedDrop {
    TimedDrop {
        id: DropId::from(id),
        name: format!("Drop {id}"),
        benefits: vec![],
        required_minutes: required,
        current_minutes: current,
        claim_id: None,
        is_claimed: false,
        precondition_ids: vec![],
    }
}
