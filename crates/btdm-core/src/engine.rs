//! The mining loop.
//!
//! Generic over [`TwitchApi`] so the whole thing runs against a scripted fake.
//! Selection here is deliberately simple — the first eligible, server-verified
//! channel. Pluggable strategies replace [`Engine::select_channel`] later; this
//! layer only has to mine *correctly*, not cleverly.

use crate::api::{ApiError, DropProgress, TwitchApi};
use crate::eligibility;
use crate::event::{EventKind, MinerEvent, RejectReason};
use crate::model::*;
use crate::progress::ProgressTracker;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub watch_interval: Duration,
    pub poll_interval: Duration,
    /// How many channels to consider per game.
    pub channel_limit: u32,
    /// Upper bound on per-campaign detail requests during a sync, so a huge
    /// campaign list cannot turn startup into a request storm.
    pub max_detail_fetches: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            watch_interval: Duration::seconds(60),
            poll_interval: Duration::seconds(120),
            channel_limit: 30,
            max_detail_fetches: 60,
        }
    }
}

/// What the engine is currently doing, for display and for the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiningState {
    Idle,
    /// Watching, but Twitch has not confirmed any progress yet.
    Establishing,
    Mining,
}

pub struct Engine<A: TwitchApi> {
    api: A,
    config: EngineConfig,
    campaigns: Vec<DropCampaign>,
    current: Option<(Channel, Stream, CampaignId)>,
    progress: ProgressTracker,
    events: Vec<MinerEvent>,
    last_watch: Option<DateTime<Utc>>,
    last_poll: Option<DateTime<Utc>>,
}

impl<A: TwitchApi> Engine<A> {
    pub fn new(api: A, config: EngineConfig) -> Self {
        Self {
            api,
            config,
            campaigns: Vec::new(),
            current: None,
            progress: ProgressTracker::new(),
            events: Vec::new(),
            last_watch: None,
            last_poll: None,
        }
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    pub fn campaigns(&self) -> &[DropCampaign] {
        &self.campaigns
    }

    pub fn current_channel(&self) -> Option<&Channel> {
        self.current.as_ref().map(|(c, _, _)| c)
    }

    pub fn drain_events(&mut self) -> Vec<MinerEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn since_last_confirmation(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.progress.since_last_confirmation(now)
    }

    pub fn state(&self) -> MiningState {
        match (&self.current, self.progress.has_confirmation()) {
            (None, _) => MiningState::Idle,
            (Some(_), false) => MiningState::Establishing,
            (Some(_), true) => MiningState::Mining,
        }
    }

    /// Confirmed progress for a drop, if Twitch has reported any.
    pub fn confirmed(&self, drop: &DropId) -> Option<&crate::progress::ConfirmedProgress> {
        self.progress.confirmed(drop)
    }

    /// Leaves the current channel. Progress there tells us nothing about the next.
    pub fn switch_away(&mut self, now: DateTime<Utc>) {
        if let Some((channel, _, _)) = &self.current {
            let id = channel.id.clone();
            self.emit(now, EventKind::ChannelOffline { channel: id });
        }
        self.current = None;
        self.progress.reset();
        self.last_watch = None;
        self.last_poll = None;
    }

    fn emit(&mut self, at: DateTime<Utc>, kind: EventKind) {
        self.events.push(MinerEvent { at, kind });
    }

    /// Refresh campaigns from Twitch and report which are actually mineable.
    ///
    /// Three sources are merged, because none is sufficient alone:
    /// * the inventory holds campaigns already in progress, with drop tiers and
    ///   confirmed minutes;
    /// * the dashboard lists *all* offered campaigns, but without drop detail;
    /// * per-campaign details fill in tiers for anything not yet started.
    ///
    /// Using only the inventory — which is the obvious shortcut — yields a
    /// miner that can continue campaigns but can never begin one.
    pub async fn sync(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let mut merged: HashMap<CampaignId, DropCampaign> = self
            .api
            .fetch_inventory()
            .await?
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let in_progress = merged.len();

        // The dashboard is advisory: if it fails we still mine what we know.
        let offered = match self.api.fetch_campaigns().await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %e, "campaign dashboard unavailable; using inventory only");
                Vec::new()
            }
        };

        let mut fetched = 0usize;
        for campaign in offered {
            if merged.contains_key(&campaign.id) {
                continue;
            }
            // Skip anything already outside its window before spending a request.
            if now >= campaign.ends_at {
                continue;
            }
            if fetched >= self.config.max_detail_fetches {
                tracing::warn!(
                    limit = self.config.max_detail_fetches,
                    "hit the campaign detail fetch cap; some campaigns were not expanded"
                );
                break;
            }
            fetched += 1;

            match self.api.campaign_details(&campaign.id).await {
                Ok(Some(full)) => {
                    merged.insert(full.id.clone(), full);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(campaign = %campaign.id, error = %e, "campaign details failed");
                }
            }
        }

        let total = merged.len();
        tracing::debug!(
            total,
            in_progress,
            expanded = fetched,
            "merged campaign sources"
        );

        let mut eligible = Vec::new();
        for campaign in merged.into_values() {
            match eligibility::assess(&campaign, now) {
                Ok(()) => eligible.push(campaign),
                Err(reason) => self.emit(
                    now,
                    EventKind::CampaignRejected {
                        campaign: campaign.id.clone(),
                        reason,
                    },
                ),
            }
        }

        // Deterministic order: soonest deadline first, so behaviour is
        // reproducible rather than dependent on hash iteration order.
        eligible.sort_by(|a, b| a.ends_at.cmp(&b.ends_at).then(a.id.cmp(&b.id)));

        self.emit(
            now,
            EventKind::CampaignsSynced {
                total,
                eligible: eligible.len(),
            },
        );
        self.campaigns = eligible;
        Ok(())
    }

    /// Picks a channel that Twitch confirms can progress one of our drops.
    ///
    /// The `available_drops` check is the load-bearing part: without it we
    /// would happily watch a channel that earns nothing, which is the failure
    /// users experience as a progress bar that never moves.
    ///
    /// It is not an absolute veto, though. If *every* candidate is rejected by
    /// it, we fall back to channels the directory tagged as drops-enabled and
    /// say so loudly. Trusting the check absolutely would turn one bad Twitch
    /// response into a miner that mines nothing at all — worse than the bug
    /// it exists to prevent.
    pub async fn select_channel(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<Option<(Channel, Stream, CampaignId)>, ApiError> {
        let campaigns = self.campaigns.clone();
        let mut fallbacks: Vec<(Channel, Stream, CampaignId)> = Vec::new();

        for campaign in &campaigns {
            let wanted: std::collections::HashSet<DropId> = eligibility::mineable_drops(campaign)
                .into_iter()
                .map(|d| d.id.clone())
                .collect();
            if wanted.is_empty() {
                continue;
            }

            let channels = self
                .api
                .live_channels(&campaign.game, self.config.channel_limit)
                .await?;
            if channels.is_empty() {
                self.emit(
                    now,
                    EventKind::CampaignRejected {
                        campaign: campaign.id.clone(),
                        reason: RejectReason::NoEligibleChannel,
                    },
                );
                continue;
            }

            let allowed: std::collections::HashSet<ChannelId> = campaign
                .allowed_channels
                .iter()
                .map(|c| c.id.clone())
                .collect();

            for (channel, stream) in channels {
                // An allow-list, when present, is absolute.
                if !allowed.is_empty() && !allowed.contains(&channel.id) {
                    continue;
                }

                match self.api.available_drops(&channel.id).await {
                    Ok(available) if available.iter().any(|d| wanted.contains(d)) => {
                        self.begin_watching(now, channel, stream, campaign.id.clone());
                        return Ok(self.current.clone());
                    }
                    Ok(_) => {
                        if stream.drops_enabled {
                            fallbacks.push((channel, stream, campaign.id.clone()));
                        }
                    }
                    Err(e) => {
                        // A failed check must not silently exclude a channel.
                        tracing::warn!(channel = %channel.login, error = %e, "eligibility check failed");
                        if stream.drops_enabled {
                            fallbacks.push((channel, stream, campaign.id.clone()));
                        }
                    }
                }
            }

            self.emit(
                now,
                EventKind::CampaignRejected {
                    campaign: campaign.id.clone(),
                    reason: RejectReason::NoEligibleChannel,
                },
            );
        }

        if let Some((channel, stream, campaign)) = fallbacks.into_iter().next() {
            tracing::warn!(
                channel = %channel.login,
                "no channel passed the drop-availability check; falling back to a \
                 directory drops-enabled tag. If progress does not confirm, this \
                 channel genuinely earns nothing."
            );
            self.begin_watching(now, channel, stream, campaign);
            return Ok(self.current.clone());
        }

        Ok(None)
    }

    fn begin_watching(
        &mut self,
        now: DateTime<Utc>,
        channel: Channel,
        stream: Stream,
        campaign: CampaignId,
    ) {
        self.progress.reset();
        self.progress.begin(now);
        self.last_watch = None;
        self.last_poll = None;
        self.current = Some((channel.clone(), stream, campaign.clone()));
        self.emit(now, EventKind::NowWatching { channel, campaign });
    }

    /// One iteration: claim anything claimable, send a heartbeat if due, then
    /// poll progress as a backstop to the websocket.
    pub async fn tick(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        self.claim_ready(now).await?;

        let Some((channel, stream, _)) = self.current.clone() else {
            return Ok(());
        };

        let watch_due = self
            .last_watch
            .is_none_or(|last| now - last >= self.config.watch_interval);
        if watch_due {
            match self.api.send_watch(&channel, &stream).await {
                Ok(()) => {
                    self.last_watch = Some(now);
                    self.emit(
                        now,
                        EventKind::WatchSent {
                            channel: channel.id.clone(),
                        },
                    );
                }
                Err(e) => {
                    self.emit(
                        now,
                        EventKind::WatchFailed {
                            channel: channel.id.clone(),
                            error: e.to_string(),
                        },
                    );
                    return Err(e);
                }
            }
        }

        // The websocket is the primary progress source; this poll is the
        // backstop for a socket that is connected but silent, so it runs on
        // its own slower interval rather than on every heartbeat.
        let poll_due = self
            .last_poll
            .is_none_or(|last| now - last >= self.config.poll_interval);
        if poll_due {
            self.last_poll = Some(now);
            if let Some(p) = self.api.current_progress(&channel.id).await? {
                self.on_pushed_progress(p, now);
            }
        }
        Ok(())
    }

    /// Records confirmed progress, whether it arrived by websocket or by poll.
    pub fn on_pushed_progress(&mut self, p: DropProgress, now: DateTime<Utc>) {
        if !self.progress.record(p.clone(), now) {
            return;
        }
        // Keep the campaign view in step so claim detection and remaining-time
        // figures reflect what the server actually reported.
        for campaign in &mut self.campaigns {
            if let Some(d) = campaign.drops.iter_mut().find(|d| d.id == p.drop_id) {
                d.current_minutes = d.current_minutes.max(p.current_minutes);
            }
        }
        self.emit(
            now,
            EventKind::ProgressConfirmed {
                drop: p.drop_id,
                current_minutes: p.current_minutes,
                required_minutes: p.required_minutes,
            },
        );
    }

    /// Marks a drop as claimable, as reported over the websocket.
    pub fn on_drop_claimable(&mut self, drop_id: &DropId, claim_id: &str) {
        for campaign in &mut self.campaigns {
            if let Some(d) = campaign.drops.iter_mut().find(|d| &d.id == drop_id) {
                d.claim_id = Some(claim_id.to_owned());
            }
        }
    }

    async fn claim_ready(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let ready: Vec<(String, DropId, String)> = self
            .campaigns
            .iter()
            .flat_map(|c| &c.drops)
            .filter(|d| d.is_claimable())
            .filter_map(|d| {
                d.claim_id
                    .clone()
                    .map(|cid| (cid, d.id.clone(), d.name.clone()))
            })
            .collect();

        for (claim_id, drop_id, name) in ready {
            self.api.claim_drop(&claim_id).await?;
            for campaign in &mut self.campaigns {
                if let Some(d) = campaign.drops.iter_mut().find(|d| d.id == drop_id) {
                    d.is_claimed = true;
                    d.claim_id = None;
                }
            }
            self.emit(now, EventKind::DropClaimed { drop: drop_id, name });
        }
        Ok(())
    }
}
