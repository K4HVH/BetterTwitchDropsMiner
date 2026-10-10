//! A scriptable Twitch. Every failure the engine must survive is expressible
//! here, which is the entire reason `btdm-core` is forbidden from doing I/O.

#![allow(dead_code)]

use btdm_core::api::{ApiError, DropProgress, Inventory, TwitchApi};
use btdm_core::model::*;
use chrono::TimeZone;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeTwitch {
    /// Campaigns already in progress, with full drop detail.
    pub inventory: Mutex<Vec<DropCampaign>>,
    /// Twitch's claimed-benefit ledger: benefit id to when it was awarded.
    /// Covers the whole account, including campaigns long gone from the
    /// in-progress inventory.
    pub awarded: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
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
    /// How many times the engine said it had stopped watching.
    pub watch_stops: Mutex<usize>,
    /// Successful claims only.
    pub claims: Mutex<Vec<String>>,
    /// Every claim attempt, successful or not.
    pub claim_attempts: Mutex<usize>,
    pub point_claims: Mutex<Vec<String>>,
    /// When set, every call fails with this error.
    pub fail_with: Mutex<Option<ApiError>>,
    /// When set, only `available_drops` fails.
    pub fail_available_drops: Mutex<bool>,
    /// When set, only the campaign dashboard listing fails.
    pub fail_campaign_list: Mutex<bool>,
    /// When set, only the directory query fails — the one that answers "who is
    /// live", and so the one whose failure must never be mistaken for "nobody".
    pub fail_directory: Mutex<bool>,
    /// When set, every watch heartbeat fails. The one request that actually
    /// advances drop time, so what the miner does when it fails matters more
    /// than for any other call here.
    pub fail_watch: Mutex<bool>,
    /// Campaign ids whose detail query fails. Per campaign rather than
    /// wholesale, because the interesting case is a sync where some campaigns
    /// were described and others were not.
    pub fail_details: Mutex<std::collections::HashSet<String>>,
    /// When set, Twitch refuses every claim.
    pub fail_claims: Mutex<bool>,
    pub detail_fetches: Mutex<usize>,
    pub stream_info_calls: Mutex<usize>,
    /// Logins that are live and playing the game, but do not appear on the
    /// directory page — the small allow-listed streamers that direct lookup
    /// by name exists to find.
    pub below_the_fold: Mutex<std::collections::HashSet<String>>,
}

impl FakeTwitch {
    fn guard(&self) -> Result<(), ApiError> {
        match &*self.fail_with.lock().unwrap() {
            Some(ApiError::Unauthorized) => Err(ApiError::Unauthorized),
            Some(ApiError::RateLimited) => Err(ApiError::RateLimited),
            Some(ApiError::Network(m)) => Err(ApiError::Network(m.clone())),
            Some(ApiError::Malformed(m)) => Err(ApiError::Malformed(m.clone())),
            Some(ApiError::Refused(m)) => Err(ApiError::Refused(m.clone())),
            None => Ok(()),
        }
    }
}

impl TwitchApi for FakeTwitch {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        self.guard()?;
        if *self.fail_campaign_list.lock().unwrap() {
            return Err(ApiError::Network("campaign dashboard exploded".into()));
        }
        Ok(self.offered.lock().unwrap().clone())
    }

    async fn fetch_inventory(&self) -> Result<Inventory, ApiError> {
        self.guard()?;
        Ok(Inventory {
            campaigns: self.inventory.lock().unwrap().clone(),
            awarded: self.awarded.lock().unwrap().clone(),
        })
    }

    async fn campaign_details(&self, id: &CampaignId) -> Result<Option<DropCampaign>, ApiError> {
        self.guard()?;
        *self.detail_fetches.lock().unwrap() += 1;
        if self.fail_details.lock().unwrap().contains(&id.0) {
            return Err(ApiError::Network("HTTP 504".into()));
        }
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
        if *self.fail_directory.lock().unwrap() {
            return Err(ApiError::Network("HTTP 504".into()));
        }
        let hidden = self.below_the_fold.lock().unwrap().clone();
        Ok(self
            .channels
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.game.as_ref().is_some_and(|g| g.id == game.id))
            .filter(|(c, _)| !hidden.contains(&c.login))
            .cloned()
            .collect())
    }

    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError> {
        self.guard()?;
        *self.stream_info_calls.lock().unwrap() += 1;
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
        if *self.fail_watch.lock().unwrap() {
            return Err(ApiError::Network("spade returned HTTP 504".into()));
        }
        self.watches_sent.lock().unwrap().push(channel.id.clone());
        Ok(())
    }

    fn stop_watch(&self) {
        *self.watch_stops.lock().unwrap() += 1;
    }

    async fn current_progress(
        &self,
        _channel: &ChannelId,
    ) -> Result<Option<DropProgress>, ApiError> {
        self.guard()?;
        Ok(self.progress.lock().unwrap().clone())
    }

    async fn claim_drop(
        &self,
        drop: &DropId,
        campaign: &CampaignId,
        claim_id: Option<&str>,
    ) -> Result<(), ApiError> {
        self.guard()?;
        *self.claim_attempts.lock().unwrap() += 1;
        if *self.fail_claims.lock().unwrap() {
            return Err(ApiError::Malformed("Twitch refused the claim".into()));
        }
        // Records what the transport would have sent, so a synthesised id is
        // visible to tests.
        self.claims.lock().unwrap().push(
            claim_id
                .map(str::to_owned)
                .unwrap_or_else(|| format!("synth#{}#{}", campaign.0, drop.0)),
        );
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
        box_art_url: None,
    }
}

/// A second game, for scenarios that need two campaigns a single channel
/// cannot both advance.
///
/// One channel advances every eligible campaign for the game it is streaming —
/// that is the whole point of the channel-centric candidate model. So a test
/// that wants two campaigns to be genuinely separate has to put them in
/// different games. Several tests used to rely on the availability check to
/// keep same-game campaigns apart, which meant they were testing that check
/// rather than the thing they named.
pub fn other_game() -> Game {
    Game {
        id: GameId::from("g2"),
        name: "Rocket League".into(),
        slug: "rocket-league".into(),
        box_art_url: None,
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
    stream_of(broadcast, drops_enabled, game())
}

/// A stream of a specific game.
pub fn stream_of(broadcast: &str, drops_enabled: bool, game: Game) -> Stream {
    Stream {
        broadcast_id: broadcast.into(),
        game: Some(game),
        title: "t".into(),
        viewers: 100,
        drops_enabled,
    }
}

pub fn campaign(id: &str, linked: bool, drops: Vec<TimedDrop>) -> DropCampaign {
    campaign_of(id, linked, drops, game())
}

/// A campaign for a specific game.
pub fn campaign_of(
    id: &str,
    linked: bool,
    drops: Vec<TimedDrop>,
    game: Game,
) -> DropCampaign {
    DropCampaign {
        id: CampaignId::from(id),
        name: format!("Campaign {id}"),
        game,
        starts_at: chrono::Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
        ends_at: chrono::Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
        drops,
        is_account_linked: linked,
        allowed_channels: vec![],
        account_link_url: None,
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
        starts_at: None,
        ends_at: None,
        required_subs: 0,
    }
}
