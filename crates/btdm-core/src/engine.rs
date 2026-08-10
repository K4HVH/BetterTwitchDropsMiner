//! The mining loop.
//!
//! Generic over [`TwitchApi`] so the whole thing runs against a scripted fake.
//!
//! The engine owns the *mechanics* — sync, candidate gathering, heartbeats,
//! progress, claiming — while [`crate::strategy`] owns the *choice* of what to
//! watch and [`crate::watchdog`] owns the decision that a session has failed.
//! Keeping those separate is what lets each be tested against synthetic worlds
//! rather than a live account.

use crate::api::{ApiError, DropProgress, TwitchApi};
use crate::eligibility;
use crate::event::{EventKind, MinerEvent, RejectReason};
use crate::model::*;
use crate::progress::ProgressTracker;
use crate::snapshot::{self, RejectedView, Snapshot, Totals, WatchingView};
use crate::strategy::{self, Candidate, Decision, MiningStrategy, WorldView};
use crate::watchdog::{Quarantine, Verdict, Watchdog, WatchdogConfig};
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub watch_interval: Duration,
    pub poll_interval: Duration,
    /// How many channels to consider per game.
    pub channel_limit: u32,
    /// Upper bound on per-campaign detail requests during a sync, so a huge
    /// campaign list cannot turn startup into a request storm.
    pub max_detail_fetches: usize,
    /// Upper bound on availability checks per selection pass. Without this,
    /// 7 campaigns times 30 channels is 210 sequential requests.
    pub max_availability_checks: usize,
    /// Which selection strategy to use. See [`crate::strategy`].
    pub strategy: String,
    /// Channel logins to prefer when otherwise equal.
    pub favourites: Vec<String>,
    /// Game names in priority order.
    pub priority_games: Vec<String>,
    /// A channel login to watch exclusively, under the `manual` strategy.
    pub pinned: Option<String>,
    pub watchdog: WatchdogConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            watch_interval: Duration::seconds(60),
            poll_interval: Duration::seconds(120),
            channel_limit: 30,
            max_detail_fetches: 60,
            max_availability_checks: 40,
            strategy: "deadline".into(),
            favourites: Vec::new(),
            priority_games: Vec::new(),
            pinned: None,
            watchdog: WatchdogConfig::default(),
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
    strategy: Box<dyn MiningStrategy>,
    favourites: HashSet<String>,
    priority_games: Vec<String>,
    campaigns: Vec<DropCampaign>,
    /// Why each unavailable campaign was rejected, for the dashboard.
    rejected: Vec<RejectedView>,
    current: Option<(Channel, Stream, CampaignId)>,
    progress: ProgressTracker,
    watchdog: Watchdog,
    quarantine: Quarantine,
    events: Vec<MinerEvent>,
    last_watch: Option<DateTime<Utc>>,
    last_poll: Option<DateTime<Utc>>,
}

impl<A: TwitchApi> Engine<A> {
    pub fn new(api: A, config: EngineConfig) -> Self {
        let strategy = strategy::by_name(&config.strategy).unwrap_or_else(|| {
            tracing::warn!(
                requested = config.strategy,
                "unknown strategy; falling back to `deadline`"
            );
            Box::new(strategy::Deadline)
        });
        let favourites = config
            .favourites
            .iter()
            .map(|f| f.to_lowercase())
            .collect();
        let priority_games = config
            .priority_games
            .iter()
            .map(|g| g.to_lowercase())
            .collect();

        Self {
            api,
            watchdog: Watchdog::new(config.watchdog),
            quarantine: Quarantine::new(Duration::minutes(5), Duration::hours(2)),
            strategy,
            favourites,
            priority_games,
            config,
            campaigns: Vec::new(),
            rejected: Vec::new(),
            current: None,
            progress: ProgressTracker::new(),
            events: Vec::new(),
            last_watch: None,
            last_poll: None,
        }
    }

    pub fn strategy_name(&self) -> &'static str {
        self.strategy.name()
    }

    /// A serialisable view of the current state, for the dashboard.
    ///
    /// Rejected campaigns are recomputed here rather than replayed from the
    /// event log, so the dashboard always shows the *current* reason a
    /// campaign is unavailable rather than a stale one.
    pub fn snapshot(&self, account: &str, login: Option<&str>, now: DateTime<Utc>) -> Snapshot {
        let watching = self.current.as_ref().map(|(channel, _, campaign)| {
            let active = self.progress.active();
            let drop_name = active.and_then(|(id, _)| {
                self.campaigns
                    .iter()
                    .flat_map(|c| &c.drops)
                    .find(|d| &d.id == id)
                    .map(|d| d.name.clone())
            });
            let since = self.progress.since_last_confirmation(now);

            WatchingView {
                channel_login: channel.login.clone(),
                channel_display_name: channel.display_name.clone(),
                campaign: self
                    .campaigns
                    .iter()
                    .find(|c| &c.id == campaign)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| campaign.0.clone()),
                drop_name,
                current_minutes: active.map(|(_, p)| p.current_minutes),
                required_minutes: active.map(|(_, p)| p.required_minutes),
                seconds_since_confirmation: since.map(|d| d.num_seconds()),
                stalled: since.is_some_and(|d| d >= self.config.watchdog.stalled_after),
            }
        });

        let eligible: Vec<_> = self
            .campaigns
            .iter()
            .map(|c| snapshot::campaign_view(c, now))
            .collect();

        let totals = Totals {
            campaigns_known: self.campaigns.len() + self.rejected.len(),
            campaigns_eligible: self.campaigns.len(),
            drops_claimed: self
                .campaigns
                .iter()
                .flat_map(|c| &c.drops)
                .filter(|d| d.is_claimed)
                .count(),
            drops_remaining: self
                .campaigns
                .iter()
                .flat_map(|c| &c.drops)
                .filter(|d| !d.is_claimed)
                .count(),
        };

        Snapshot {
            account: account.to_owned(),
            login: login.map(str::to_owned),
            strategy: self.strategy.name().to_owned(),
            state: format!("{:?}", self.state()),
            generated_at: now,
            watching,
            eligible,
            rejected: self.rejected.clone(),
            totals,
        }
    }

    /// Ask the watchdog whether the current session is healthy.
    pub fn assess_health(&mut self, now: DateTime<Utc>) -> Verdict {
        let since = self
            .current
            .as_ref()
            .and(self.progress.since_last_confirmation(now));
        self.watchdog.assess(since, now)
    }

    /// Quarantine the current channel so selection stops picking it, with
    /// exponential backoff so it is retried later rather than abandoned.
    pub fn quarantine_current(&mut self, now: DateTime<Utc>) {
        if let Some((channel, _, _)) = &self.current {
            let id = channel.id.0.clone();
            let login = channel.login.clone();
            self.quarantine.punish(&id, now);
            tracing::warn!(channel = %login, "quarantined after failing to make progress");
        }
        self.switch_away(now);
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    /// Rung 1: send a heartbeat on the next tick regardless of the interval.
    pub fn force_next_watch(&mut self) {
        self.last_watch = None;
    }

    /// Rung 2: re-fetch stream info.
    ///
    /// The broadcast ID changes when a stream restarts, and heartbeats carrying
    /// a stale one are accepted and earn nothing — a healthy-looking session
    /// that is silently worthless. Returns whether the ID actually changed.
    pub async fn refresh_stream_info(&mut self, now: DateTime<Utc>) -> Result<bool, ApiError> {
        let Some((channel, old, campaign)) = self.current.clone() else {
            return Ok(false);
        };

        match self.api.stream_info(&channel.login).await? {
            Some(fresh) => {
                let changed = fresh.broadcast_id != old.broadcast_id;
                if changed {
                    tracing::warn!(
                        channel = %channel.login,
                        old = %old.broadcast_id,
                        new = %fresh.broadcast_id,
                        "the stream restarted; heartbeats were carrying a stale broadcast id"
                    );
                    self.progress.begin(now);
                }
                self.current = Some((channel, fresh, campaign));
                Ok(changed)
            }
            None => {
                tracing::info!(channel = %channel.login, "channel is offline");
                self.switch_away(now);
                Ok(true)
            }
        }
    }

    /// Rung 3: ask Twitch again whether this channel progresses anything of
    /// ours. Returns false when it definitively does not.
    pub async fn reverify_current_channel(&mut self) -> Result<bool, ApiError> {
        let Some((channel, _, campaign_id)) = self.current.clone() else {
            return Ok(false);
        };
        let Some(campaign) = self.campaigns.iter().find(|c| c.id == campaign_id) else {
            return Ok(false);
        };

        let wanted: HashSet<DropId> = eligibility::mineable_drops(campaign)
            .into_iter()
            .map(|d| d.id.clone())
            .collect();

        match self.api.available_drops(&channel.id).await {
            Ok(available) => Ok(available.iter().any(|d| wanted.contains(d))),
            // An inconclusive check is not proof of failure.
            Err(e) => {
                tracing::warn!(error = %e, "re-verification inconclusive");
                Ok(true)
            }
        }
    }

    /// Rung 7: stop trying this campaign for a while.
    pub fn quarantine_current_campaign(&mut self, now: DateTime<Utc>) {
        if let Some((_, _, campaign)) = &self.current {
            let id = campaign.0.clone();
            self.quarantine.punish(&format!("campaign:{id}"), now);
            tracing::error!(
                campaign = %id,
                "recovery exhausted; quarantining this campaign and moving on"
            );
        }
        self.quarantine_current(now);
    }

    pub fn is_campaign_quarantined(&self, id: &CampaignId, now: DateTime<Utc>) -> bool {
        self.quarantine
            .is_quarantined(&format!("campaign:{}", id.0), now)
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

    /// The drop currently advancing, with its server-confirmed figures.
    /// This is the only drop whose numbers may be displayed as "current".
    pub fn active_drop(&self) -> Option<(&DropId, &crate::progress::ConfirmedProgress)> {
        self.progress.active()
    }

    /// Leaves the current channel. Progress there tells us nothing about the next.
    pub fn switch_away(&mut self, now: DateTime<Utc>) {
        if let Some((channel, _, _)) = &self.current {
            let id = channel.id.clone();
            self.emit(now, EventKind::ChannelOffline { channel: id });
        }
        self.current = None;
        self.progress.reset();
        self.watchdog.reset();
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
        let mut skipped_unlinked = 0usize;
        for campaign in offered {
            if merged.contains_key(&campaign.id) {
                continue;
            }
            // Skip anything already outside its window before spending a request.
            if now >= campaign.ends_at {
                continue;
            }
            // The dashboard already reports the account link, and an unlinked
            // campaign can never be mined. Filtering here turned 130 detail
            // requests into a handful against a real account, which is what
            // made the fetch cap stop truncating the campaign list.
            if !campaign.is_account_linked {
                skipped_unlinked += 1;
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
            skipped_unlinked,
            "merged campaign sources"
        );

        let mut eligible = Vec::new();
        let mut rejected = Vec::new();
        for campaign in merged.into_values() {
            match eligibility::assess(&campaign, now) {
                Ok(()) => eligible.push(campaign),
                Err(reason) => {
                    rejected.push(RejectedView {
                        id: campaign.id.0.clone(),
                        name: campaign.name.clone(),
                        game: campaign.game.name.clone(),
                        reason: format!("{reason:?}"),
                    });
                    self.emit(
                        now,
                        EventKind::CampaignRejected {
                            campaign: campaign.id.clone(),
                            reason,
                        },
                    );
                }
            }
        }
        self.rejected = rejected;

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
        self.quarantine.prune(now);
        let (candidates, fallbacks) = self.gather_candidates(now).await?;

        let current_id = self.current.as_ref().map(|(c, _, _)| c.id.clone());
        let decision = {
            let world = WorldView {
                now,
                campaigns: &self.campaigns,
                candidates: &candidates,
                favourites: &self.favourites,
                priority_games: &self.priority_games,
                pinned: self.config.pinned.as_deref(),
                current: current_id.as_ref(),
            };
            strategy::choose(self.strategy.as_ref(), &world)
        };

        match decision {
            Decision::Watch(c) => {
                self.begin_watching(now, c.channel, c.stream, c.campaign);
                return Ok(self.current.clone());
            }
            Decision::Idle(reason) => {
                // Staying put is a decision, not a failure.
                if self.current.is_some() {
                    tracing::debug!(reason, "keeping the current channel");
                    return Ok(self.current.clone());
                }
                tracing::info!(reason, "no channel selected");
            }
        }

        // Nothing passed the strict availability check. Falling back beats
        // mining nothing at all, but it is announced rather than silent.
        if candidates.is_empty()
            && let Some(c) = fallbacks.into_iter().next()
        {
            tracing::warn!(
                channel = %c.channel.login,
                "no channel passed the drop-availability check; falling back to a \
                 directory drops-enabled tag. If progress does not confirm, this \
                 channel genuinely earns nothing and the watchdog will move on."
            );
            self.begin_watching(now, c.channel, c.stream, c.campaign);
            return Ok(self.current.clone());
        }

        Ok(None)
    }

    /// Builds the candidate set the strategy chooses from.
    ///
    /// Returns `(verified, fallbacks)`: candidates Twitch confirmed can
    /// progress our drops, and drops-enabled channels that failed or were
    /// excluded by that check.
    async fn gather_candidates(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<(Vec<Candidate>, Vec<Candidate>), ApiError> {
        let campaigns = self.campaigns.clone();
        let mut verified = Vec::new();
        let mut fallbacks = Vec::new();
        let mut checks = 0usize;

        for campaign in &campaigns {
            let wanted: HashSet<DropId> = eligibility::mineable_drops(campaign)
                .into_iter()
                .map(|d| d.id.clone())
                .collect();
            if wanted.is_empty() {
                continue;
            }
            // A campaign that exhausted the recovery ladder is rested, not
            // retried immediately.
            if self.is_campaign_quarantined(&campaign.id, now) {
                continue;
            }
            // Do not spend requests on a campaign that cannot finish anyway.
            if !strategy::is_feasible(campaign, now) && self.config.pinned.is_none() {
                self.emit(
                    now,
                    EventKind::CampaignRejected {
                        campaign: campaign.id.clone(),
                        reason: RejectReason::Infeasible,
                    },
                );
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

            let allowed: HashSet<ChannelId> = campaign
                .allowed_channels
                .iter()
                .map(|c| c.id.clone())
                .collect();

            for (channel, stream) in channels {
                // An allow-list, when present, is absolute.
                if !allowed.is_empty() && !allowed.contains(&channel.id) {
                    continue;
                }
                if self.quarantine.is_quarantined(&channel.id.0, now) {
                    continue;
                }
                if checks >= self.config.max_availability_checks {
                    tracing::debug!(
                        limit = self.config.max_availability_checks,
                        "availability-check budget reached for this pass"
                    );
                    break;
                }
                checks += 1;

                let make = |drops: Vec<DropId>| Candidate {
                    channel: channel.clone(),
                    stream: stream.clone(),
                    campaign: campaign.id.clone(),
                    drops,
                };

                match self.api.available_drops(&channel.id).await {
                    Ok(available) => {
                        let matched: Vec<DropId> = available
                            .into_iter()
                            .filter(|d| wanted.contains(d))
                            .collect();
                        if !matched.is_empty() {
                            verified.push(make(matched));
                        } else if stream.drops_enabled {
                            fallbacks.push(make(wanted.iter().cloned().collect()));
                        }
                    }
                    Err(e) => {
                        // A failed check must not silently exclude a channel.
                        tracing::warn!(
                            channel = %channel.login,
                            error = %e,
                            "availability check failed; keeping as a fallback"
                        );
                        if stream.drops_enabled {
                            fallbacks.push(make(wanted.iter().cloned().collect()));
                        }
                    }
                }
            }
        }

        Ok((verified, fallbacks))
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

    /// Whether a drop belongs to the campaign currently being watched.
    ///
    /// Twitch reports drop events per *account*, not per session. If the user
    /// is also watching in a browser, those events arrive here too. Letting
    /// them count would reset the stall clock while this miner earned nothing
    /// — the precise failure this project exists to prevent, so the check is
    /// deliberately strict.
    fn is_progress_for_current_campaign(&self, drop: &DropId) -> bool {
        let Some((_, _, campaign_id)) = &self.current else {
            return false;
        };
        self.campaigns
            .iter()
            .find(|c| &c.id == campaign_id)
            .is_some_and(|c| c.drops.iter().any(|d| &d.id == drop))
    }

    /// Records confirmed progress, whether it arrived by websocket or by poll.
    ///
    /// Progress for anything other than the campaign being watched is ignored.
    pub fn on_pushed_progress(&mut self, p: DropProgress, now: DateTime<Utc>) {
        if !self.is_progress_for_current_campaign(&p.drop_id) {
            tracing::debug!(
                drop = %p.drop_id,
                "ignoring progress for a drop this session is not mining"
            );
            return;
        }
        if !self.progress.record(p.clone(), now) {
            return;
        }
        // Real progress ends any stall episode in flight.
        self.watchdog.on_progress();
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
