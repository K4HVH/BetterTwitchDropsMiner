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
use crate::control::{Command, CommandEffect, PersistedSettings};
use crate::eligibility;
use crate::event::{EventKind, MinerEvent, RejectReason, Severity, StopReason};
use crate::model::*;
use crate::progress::ProgressTracker;
use crate::snapshot::{
    self, HealthView, LiveChannelView, QuarantineView, RejectedView, SettingsView, Snapshot,
    StrategyView, Totals, ViewContext, WatchingView,
};
use crate::strategy::{self, Advance, Candidate, Decision, MiningStrategy, WorldView};
use crate::watchdog::{Quarantine, RecoveryStep, Verdict, Watchdog, WatchdogConfig};
use futures_util::StreamExt;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub watch_interval: Duration,
    pub poll_interval: Duration,
    /// How often campaigns are re-fetched from Twitch.
    pub sync_interval: Duration,
    /// How often the choice of channel is reconsidered while already watching.
    /// Without this the miner only ever chooses when idle, so a campaign that
    /// becomes urgent is ignored until the current stream ends.
    pub reselect_interval: Duration,
    /// How many channels to consider per game.
    pub channel_limit: u32,
    /// Upper bound on per-campaign detail requests during a sync, so a huge
    /// campaign list cannot turn startup into a request storm.
    pub max_detail_fetches: usize,
    /// Upper bound on availability checks per selection pass. Checks are now
    /// deduplicated per channel rather than per channel-campaign pair, so this
    /// budget goes much further than it used to.
    pub max_availability_checks: usize,
    /// How many availability checks run at once.
    pub check_concurrency: usize,
    /// How many live channels to verify per campaign. Beyond a handful of
    /// working options the extra requests buy nothing — the strategy only
    /// picks one.
    pub max_channels_per_campaign: usize,
    /// How deep into a game's directory to look when a campaign restricts
    /// itself to specific channels. Allow-listed streamers are usually small,
    /// so the default page of most-relevant channels rarely contains them.
    pub allow_list_channel_limit: u32,
    /// An allow-list no longer than this is resolved channel by channel, which
    /// finds streamers the directory never surfaces at all. Above it, that
    /// would be hundreds of requests, so the deeper directory page is used
    /// instead.
    pub max_direct_allow_list_lookups: usize,
    /// Which selection strategy to use. See [`crate::strategy`].
    pub strategy: String,
    /// Channel logins to prefer when otherwise equal.
    pub favourites: Vec<String>,
    /// Game names in priority order.
    pub priority_games: Vec<String>,
    /// A channel login to watch exclusively, under the `manual` strategy.
    pub pinned: Option<String>,
    /// Campaign ids the user never wants mined.
    pub excluded_campaigns: Vec<String>,
    /// Game names the user never wants mined.
    pub excluded_games: Vec<String>,
    pub paused: bool,
    pub watchdog: WatchdogConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            watch_interval: Duration::seconds(60),
            poll_interval: Duration::seconds(120),
            sync_interval: Duration::minutes(10),
            reselect_interval: Duration::minutes(10),
            channel_limit: 30,
            max_detail_fetches: 60,
            max_availability_checks: 160,
            check_concurrency: 8,
            max_channels_per_campaign: 8,
            allow_list_channel_limit: 100,
            max_direct_allow_list_lookups: 8,
            strategy: "deadline".into(),
            favourites: Vec::new(),
            priority_games: Vec::new(),
            pinned: None,
            excluded_campaigns: Vec::new(),
            excluded_games: Vec::new(),
            paused: false,
            watchdog: WatchdogConfig::default(),
        }
    }
}

/// What a recovery rung asks of the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryOutcome {
    /// Campaigns should be re-fetched before the next selection.
    pub resync: bool,
    /// The caller should check the token; the engine cannot.
    pub validate_auth: bool,
}

/// What the engine is currently doing, for display and for the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiningState {
    Idle,
    Paused,
    /// Watching, but Twitch has not confirmed any progress yet.
    Establishing,
    Mining,
}

/// The session in progress: one channel, and everything it is earning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentWatch {
    pub channel: Channel,
    pub stream: Stream,
    /// Campaigns this channel advances, most urgent first. More than one is
    /// the normal case, not an exception.
    pub campaigns: Vec<CampaignId>,
    /// Drops being advanced, across all of those campaigns.
    pub drops: Vec<DropId>,
    pub since: DateTime<Utc>,
    /// Why the strategy chose this channel.
    pub reason: String,
}

impl CurrentWatch {
    pub fn primary_campaign(&self) -> Option<&CampaignId> {
        self.campaigns.first()
    }
}

pub struct Engine<A: TwitchApi> {
    api: A,
    config: EngineConfig,
    strategy: Box<dyn MiningStrategy>,
    // Live settings. These start from the config but are mutable at runtime,
    // because every one of them is something the dashboard can change.
    favourites: HashSet<String>,
    priority_games: Vec<String>,
    pinned: Option<String>,
    excluded_campaigns: HashSet<String>,
    excluded_games: HashSet<String>,
    paused: bool,
    /// A channel the user asked to watch next, honoured once.
    forced_channel: Option<String>,
    campaigns: Vec<DropCampaign>,
    /// Campaigns that have ended but whose earned drops can still be claimed.
    /// Twitch allows claiming for 24 hours after a campaign closes; dropping
    /// them at the moment they expired threw away rewards already worked for.
    recently_ended: Vec<DropCampaign>,
    /// Why each unavailable campaign was rejected, for the dashboard.
    rejected: Vec<RejectedView>,
    current: Option<CurrentWatch>,
    /// The candidate set from the last selection pass, so the dashboard can
    /// show what was actually on offer rather than only what won.
    live: Vec<Candidate>,
    progress: ProgressTracker,
    watchdog: Watchdog,
    quarantine: Quarantine,
    events: Vec<MinerEvent>,
    last_watch: Option<DateTime<Utc>>,
    last_poll: Option<DateTime<Utc>>,
    last_sync: Option<DateTime<Utc>>,
    /// When this channel last proved it is earning the campaign it was chosen
    /// for. Separate from the display clock on purpose — see
    /// [`Engine::counts_towards_health`].
    last_health_advance: Option<DateTime<Utc>>,
    /// Set by the user's "sync now", cleared by the next sync.
    sync_forced: bool,
    last_selection: Option<DateTime<Utc>>,
    /// A command asked for the choice to be reconsidered now.
    reselect_forced: bool,
    /// The rung being worked, for display.
    recovery_step: Option<RecoveryStep>,
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
        let favourites = config.favourites.iter().map(|f| f.to_lowercase()).collect();
        let priority_games = config
            .priority_games
            .iter()
            .map(|g| g.to_lowercase())
            .collect();
        let excluded_campaigns = config.excluded_campaigns.iter().cloned().collect();
        let excluded_games = config
            .excluded_games
            .iter()
            .map(|g| g.to_lowercase())
            .collect();
        let pinned = config.pinned.clone();
        let paused = config.paused;

        Self {
            api,
            watchdog: Watchdog::new(config.watchdog),
            quarantine: Quarantine::new(Duration::minutes(5), Duration::hours(2)),
            strategy,
            favourites,
            priority_games,
            pinned,
            excluded_campaigns,
            excluded_games,
            paused,
            forced_channel: None,
            config,
            campaigns: Vec::new(),
            recently_ended: Vec::new(),
            rejected: Vec::new(),
            current: None,
            live: Vec::new(),
            progress: ProgressTracker::new(),
            events: Vec::new(),
            last_watch: None,
            last_poll: None,
            last_sync: None,
            last_health_advance: None,
            sync_forced: false,
            last_selection: None,
            reselect_forced: false,
            recovery_step: None,
        }
    }

    pub fn strategy_name(&self) -> &'static str {
        self.strategy.name()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Applies a user command. Everything here takes effect immediately; none
    /// of it waits for the next sync or a restart.
    pub fn apply_command(&mut self, cmd: Command, now: DateTime<Utc>) -> CommandEffect {
        let effect = self.apply_command_inner(cmd, now);
        if !effect.message.is_empty() {
            self.emit(
                now,
                EventKind::CommandApplied {
                    message: effect.message.clone(),
                },
            );
        }
        effect
    }

    fn apply_command_inner(&mut self, cmd: Command, now: DateTime<Utc>) -> CommandEffect {
        match cmd {
            Command::Pause => {
                if self.paused {
                    return CommandEffect::message("Already paused");
                }
                self.paused = true;
                self.stop_watching(now, StopReason::UserRequest);
                self.emit(now, EventKind::Paused);
                CommandEffect::message("Mining paused").with_persist()
            }
            Command::Resume => {
                if !self.paused {
                    return CommandEffect::message("Already running");
                }
                self.paused = false;
                self.reselect_forced = true;
                self.emit(now, EventKind::Resumed);
                CommandEffect::message("Mining resumed")
                    .with_persist()
                    .with_reselect()
            }
            Command::SetStrategy { name } => match strategy::by_name(&name) {
                Some(s) => {
                    let label = s.name();
                    self.strategy = s;
                    self.config.strategy = label.to_owned();
                    // Reconsider at once, but do not abandon the current
                    // channel outright: if the new strategy would pick it
                    // anyway, dropping the session would throw away minutes
                    // for nothing.
                    self.reselect_forced = true;
                    CommandEffect::message(format!("Strategy set to {label}"))
                        .with_persist()
                        .with_reselect()
                }
                None => CommandEffect::message(format!("No such strategy: {name}")),
            },
            Command::Pin { channel } => {
                let login = channel.map(|c| c.trim().to_lowercase()).filter(|c| !c.is_empty());
                let msg = match &login {
                    Some(c) => format!("Pinned {c}"),
                    None => "Pin cleared".to_string(),
                };
                self.pinned = login;
                self.reselect_forced = true;
                CommandEffect::message(msg).with_persist().with_reselect()
            }
            Command::SetFavourite { login, on } => {
                let key = login.trim().to_lowercase();
                if key.is_empty() {
                    return CommandEffect::message("A channel name is required");
                }
                if on {
                    self.favourites.insert(key.clone());
                } else {
                    self.favourites.remove(&key);
                }
                CommandEffect::message(if on {
                    format!("{key} added to favourites")
                } else {
                    format!("{key} removed from favourites")
                })
                .with_persist()
            }
            Command::SetPriorityGames { games } => {
                self.priority_games = games
                    .iter()
                    .map(|g| g.trim().to_lowercase())
                    .filter(|g| !g.is_empty())
                    .collect();
                let n = self.priority_games.len();
                self.reselect_forced = true;
                CommandEffect::message(format!("Priority list set to {n} game(s)"))
                    .with_persist()
                    .with_reselect()
            }
            Command::SetCampaignExcluded { campaign, excluded } => {
                let name = self
                    .campaigns
                    .iter()
                    .find(|c| c.id == campaign)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| campaign.0.clone());
                if excluded {
                    self.excluded_campaigns.insert(campaign.0.clone());
                    // Trim it out of the session rather than ending the whole
                    // thing. One channel commonly advances several campaigns,
                    // and excluding one of them is no reason to throw away the
                    // confirmed progress on the others — which is what
                    // stopping would do, since it resets the progress tracker
                    // and the stall clock.
                    if let Some(w) = &mut self.current {
                        w.campaigns.retain(|id| id != &campaign);
                        if w.campaigns.is_empty() {
                            self.stop_watching(now, StopReason::UserRequest);
                        }
                    }
                } else {
                    self.excluded_campaigns.remove(&campaign.0);
                }
                self.reselect_forced = true;
                CommandEffect::message(if excluded {
                    format!("Excluded {name}")
                } else {
                    format!("Re-included {name}")
                })
                .with_persist()
                .with_reselect()
            }
            Command::SetGameExcluded { game, excluded } => {
                let key = game.trim().to_lowercase();
                if key.is_empty() {
                    return CommandEffect::message("A game name is required");
                }
                if excluded {
                    self.excluded_games.insert(key.clone());
                    if self.current_advances_excluded_game() {
                        self.stop_watching(now, StopReason::UserRequest);
                    }
                } else {
                    self.excluded_games.remove(&key);
                }
                self.reselect_forced = true;
                CommandEffect::message(if excluded {
                    format!("Excluded every campaign for {game}")
                } else {
                    format!("Re-included {game}")
                })
                .with_persist()
                .with_reselect()
            }
            Command::SwitchChannel => {
                if self.current.is_none() {
                    return CommandEffect::message("Not watching anything").with_reselect();
                }
                // Quarantine briefly so the very next pass does not simply
                // pick the same channel again and look like nothing happened.
                if let Some(w) = &self.current {
                    let id = w.channel.id.0.clone();
                    self.quarantine.punish(&id, now);
                }
                self.stop_watching(now, StopReason::UserRequest);
                CommandEffect::message("Switching channel").with_reselect()
            }
            Command::WatchChannel { login } => {
                let key = login.trim().to_lowercase();
                if key.is_empty() {
                    return CommandEffect::message("A channel name is required");
                }
                // An explicit request overrides an earlier penalty on that
                // channel, otherwise the button silently does nothing.
                self.quarantine.prune(now);
                self.forced_channel = Some(key.clone());
                self.stop_watching(now, StopReason::UserRequest);
                CommandEffect::message(format!("Switching to {key}")).with_reselect()
            }
            Command::SyncNow => {
                self.sync_forced = true;
                CommandEffect::message("Refreshing campaigns").with_resync()
            }
            Command::ClearQuarantine => {
                let n = self.quarantine.len();
                self.quarantine.clear();
                self.reselect_forced = true;
                CommandEffect::message(format!("Cleared {n} penalty/penalties")).with_reselect()
            }
        }
    }

    fn current_advances_excluded_game(&self) -> bool {
        let Some(w) = &self.current else {
            return false;
        };
        w.campaigns.iter().any(|id| {
            self.campaigns
                .iter()
                .find(|c| &c.id == id)
                .is_some_and(|c| self.excluded_games.contains(&c.game.name.to_lowercase()))
        })
    }

    /// The settings the caller should write back to disk.
    pub fn settings(&self) -> PersistedSettings {
        let mut favourites: Vec<String> = self.favourites.iter().cloned().collect();
        favourites.sort();
        let mut excluded_campaigns: Vec<String> = self.excluded_campaigns.iter().cloned().collect();
        excluded_campaigns.sort();
        let mut excluded_games: Vec<String> = self.excluded_games.iter().cloned().collect();
        excluded_games.sort();

        PersistedSettings {
            strategy: self.strategy.name().to_owned(),
            priority_games: self.priority_games.clone(),
            favourite_channels: favourites,
            pinned_channel: self.pinned.clone(),
            excluded_campaigns,
            excluded_games,
            paused: self.paused,
        }
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    /// A serialisable view of the current state, for the dashboard.
    ///
    /// Rejected campaigns are recomputed here rather than replayed from the
    /// event log, so the dashboard always shows the *current* reason a
    /// campaign is unavailable rather than a stale one.
    pub fn snapshot(&self, account: &str, login: Option<&str>, now: DateTime<Utc>) -> Snapshot {
        let active = self.progress.active();
        let empty: Vec<CampaignId> = Vec::new();
        let current_campaigns = self.current.as_ref().map_or(&empty, |w| &w.campaigns);

        let watching = self.current.as_ref().map(|w| {
            let drop_name = active.and_then(|(id, _)| self.drop_name(id));
            let since = self.progress.since_last_confirmation(now);

            WatchingView {
                channel_login: w.channel.login.clone(),
                channel_display_name: w.channel.display_name.clone(),
                stream_title: w.stream.title.clone(),
                viewers: w.stream.viewers,
                campaign: w
                    .primary_campaign()
                    .map(|id| self.campaign_name(id))
                    .unwrap_or_else(|| "(unknown)".into()),
                campaigns: w.campaigns.iter().map(|id| self.campaign_name(id)).collect(),
                reason: w.reason.clone(),
                since: w.since,
                drop_name,
                current_minutes: active.map(|(_, p)| p.current_minutes),
                required_minutes: active.map(|(_, p)| p.required_minutes),
                seconds_since_confirmation: since.map(|d| d.num_seconds()),
                stalled: since.is_some_and(|d| d >= self.config.watchdog.stalled_after),
            }
        });

        // How many live channels were found per campaign, so a campaign with
        // nobody streaming it can say so instead of looking merely unlucky.
        let mut live_by_campaign: HashMap<String, usize> = HashMap::new();
        for candidate in &self.live {
            for id in candidate.campaigns() {
                *live_by_campaign.entry(id.0.clone()).or_default() += 1;
            }
        }

        let ctx = ViewContext {
            now,
            excluded_campaigns: &self.excluded_campaigns,
            excluded_games: &self.excluded_games,
            current_campaigns,
            active_drop: active.map(|(id, _)| id),
            live_by_campaign: &live_by_campaign,
        };

        let eligible: Vec<_> = self
            .campaigns
            .iter()
            .map(|c| snapshot::campaign_view(c, &ctx))
            .collect();

        let current_channel = self.current.as_ref().map(|w| &w.channel.id);
        let live: Vec<LiveChannelView> = self
            .live
            .iter()
            .map(|c| LiveChannelView {
                login: c.channel.login.clone(),
                display_name: c.channel.display_name.clone(),
                viewers: c.stream.viewers,
                title: c.stream.title.clone(),
                game: c.stream.game.as_ref().map(|g| g.name.clone()),
                campaigns: c.campaigns().map(|id| self.campaign_name(id)).collect(),
                drops: c.drop_count(),
                is_current: current_channel == Some(&c.channel.id),
                is_favourite: self.favourites.contains(&c.channel.login.to_lowercase()),
            })
            .collect();

        let totals = Totals {
            campaigns_known: self.campaigns.len() + self.rejected.len(),
            campaigns_eligible: self.campaigns.len(),
            campaigns_at_risk: eligible.iter().filter(|c| c.at_risk && !c.excluded).count(),
            drops_claimed: self.all_drops().filter(|d| d.is_claimed).count(),
            drops_remaining: self.all_drops().filter(|d| !d.is_claimed).count(),
            drops_claimable: self.all_drops().filter(|d| d.is_claimable()).count(),
            minutes_remaining: self
                .campaigns
                .iter()
                .filter(|c| !ctx.is_excluded(c))
                .map(eligibility::remaining_minutes)
                .sum(),
        };

        let health = HealthView {
            verdict: match self.watchdog_verdict_label() {
                Some(v) => v,
                None => "idle".into(),
            },
            seconds_since_confirmation: self
                .progress
                .since_last_confirmation(now)
                .map(|d| d.num_seconds()),
            recovery_step: self.recovery_step.map(|s| s.describe().to_owned()),
            last_sync: self.last_sync,
            next_sync_in_seconds: self.last_sync.map(|t| {
                (t + self.config.sync_interval - now).num_seconds().max(0)
            }),
            quarantined_channels: self
                .quarantine
                .active(now)
                .into_iter()
                .filter(|(k, _)| !k.starts_with("campaign:"))
                .map(|(name, until)| QuarantineView { name, until })
                .collect(),
            quarantined_campaigns: self
                .quarantine
                .active(now)
                .into_iter()
                .filter_map(|(k, until)| {
                    k.strip_prefix("campaign:").map(|id| QuarantineView {
                        name: self.campaign_name(&CampaignId(id.to_owned())),
                        until,
                    })
                })
                .collect(),
        };

        let settings = SettingsView {
            strategy: self.strategy.name().to_owned(),
            priority_games: self.priority_games.clone(),
            favourites: {
                let mut v: Vec<String> = self.favourites.iter().cloned().collect();
                v.sort();
                v
            },
            pinned: self.pinned.clone(),
            excluded_campaigns: {
                let mut v: Vec<String> = self.excluded_campaigns.iter().cloned().collect();
                v.sort();
                v
            },
            excluded_games: {
                let mut v: Vec<String> = self.excluded_games.iter().cloned().collect();
                v.sort();
                v
            },
        };

        let strategies = strategy::ALL_STRATEGIES
            .iter()
            .map(|name| StrategyView {
                name: (*name).to_owned(),
                description: strategy::describe(name).to_owned(),
                selected: *name == self.strategy.name(),
            })
            .collect();

        Snapshot {
            account: account.to_owned(),
            login: login.map(str::to_owned),
            strategy: self.strategy.name().to_owned(),
            state: match self.state() {
                MiningState::Idle => "Idle",
                MiningState::Paused => "Paused",
                MiningState::Establishing => "Establishing",
                MiningState::Mining => "Mining",
            }
            .to_owned(),
            paused: self.paused,
            generated_at: now,
            watching,
            eligible,
            rejected: self.rejected.clone(),
            live,
            settings,
            strategies,
            health,
            totals,
        }
    }

    fn watchdog_verdict_label(&self) -> Option<String> {
        self.current.as_ref()?;
        Some(match self.recovery_step {
            Some(_) => "recovering".into(),
            None if self.progress.has_confirmation() => "healthy".into(),
            None => "establishing".into(),
        })
    }

    fn all_drops(&self) -> impl Iterator<Item = &TimedDrop> {
        self.campaigns.iter().flat_map(|c| &c.drops)
    }

    fn campaign_name(&self, id: &CampaignId) -> String {
        self.campaigns
            .iter()
            .find(|c| &c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| id.0.clone())
    }

    fn drop_name(&self, id: &DropId) -> Option<String> {
        self.all_drops().find(|d| &d.id == id).map(|d| d.name.clone())
    }

    // -----------------------------------------------------------------------
    // Health and recovery
    // -----------------------------------------------------------------------

    /// Ask the watchdog whether the current session is healthy.
    pub fn assess_health(&mut self, now: DateTime<Utc>) -> Verdict {
        let since = self.current.as_ref().and(self.since_last_confirmation(now));
        let verdict = self.watchdog.assess(since, now);
        self.recovery_step = match &verdict {
            Verdict::Escalate(step) => Some(*step),
            Verdict::Healthy => None,
            // Keep showing the last rung while waiting out the cooldown, so
            // the dashboard does not flicker between "recovering" and "fine".
            _ => self.recovery_step,
        };
        verdict
    }

    /// Quarantine the current channel so selection stops picking it, with
    /// exponential backoff so it is retried later rather than abandoned.
    pub fn quarantine_current(&mut self, now: DateTime<Utc>) {
        if let Some(w) = &self.current {
            let id = w.channel.id.0.clone();
            let login = w.channel.display_name.clone();
            let until = self.quarantine.punish(&id, now);
            tracing::warn!(channel = %login, "quarantined after failing to make progress");
            self.emit(now, EventKind::Quarantined { what: login, until });
        }
        self.stop_watching(now, StopReason::NotEarning);
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
        let Some(watch) = self.current.clone() else {
            return Ok(false);
        };

        match self.api.stream_info(&watch.channel.login).await? {
            Some(fresh) => {
                let changed = fresh.broadcast_id != watch.stream.broadcast_id;
                if changed {
                    tracing::warn!(
                        channel = %watch.channel.login,
                        old = %watch.stream.broadcast_id,
                        new = %fresh.broadcast_id,
                        "the stream restarted; heartbeats were carrying a stale broadcast id"
                    );
                    self.progress.begin(now);
                    self.last_health_advance = Some(now);
                }
                if let Some(w) = &mut self.current {
                    w.stream = fresh;
                }
                Ok(changed)
            }
            None => {
                tracing::info!(channel = %watch.channel.login, "channel is offline");
                self.stop_watching(now, StopReason::WentOffline);
                Ok(true)
            }
        }
    }

    /// Rung 3: ask Twitch again whether this channel progresses anything of
    /// ours. Returns false when it definitively does not.
    ///
    /// Checked against *every* campaign the session is advancing: a channel
    /// that stopped serving one of several is still worth watching.
    pub async fn reverify_current_channel(&mut self) -> Result<bool, ApiError> {
        let Some(watch) = self.current.clone() else {
            return Ok(false);
        };

        let wanted: HashSet<DropId> = watch
            .campaigns
            .iter()
            .filter_map(|id| self.campaigns.iter().find(|c| &c.id == id))
            .flat_map(|c| eligibility::mineable_drops(c))
            .map(|d| d.id.clone())
            .collect();
        if wanted.is_empty() {
            return Ok(false);
        }

        match self.api.available_drops(&watch.channel.id).await {
            Ok(available) => Ok(available.iter().any(|d| wanted.contains(d))),
            // An inconclusive check is not proof of failure.
            Err(e) => {
                tracing::warn!(error = %e, "re-verification inconclusive");
                Ok(true)
            }
        }
    }

    /// Works one rung of the recovery ladder.
    ///
    /// Everything the ladder does that only needs the Twitch API lives here so
    /// it can be tested against a scripted fake; the caller handles only the
    /// auth rung, which needs credentials the engine does not hold.
    pub async fn apply_recovery(
        &mut self,
        step: RecoveryStep,
        now: DateTime<Utc>,
    ) -> Result<RecoveryOutcome, ApiError> {
        tracing::warn!(?step, "no confirmed progress; working the recovery ladder");
        self.recovery_step = Some(step);
        self.emit(
            now,
            EventKind::Recovering {
                step: format!("{step:?}"),
                detail: step.describe().to_owned(),
            },
        );

        match step {
            RecoveryStep::ResendWatch => {
                self.force_next_watch();
                Ok(RecoveryOutcome::default())
            }
            RecoveryStep::RefreshStreamInfo => {
                match self.refresh_stream_info(now).await {
                    // The broadcast id moved, so heartbeats were earning nothing.
                    Ok(true) => self.force_next_watch(),
                    Ok(false) => tracing::debug!("stream info unchanged"),
                    Err(e) => tracing::warn!(error = %e, "could not refresh stream info"),
                }
                Ok(RecoveryOutcome::default())
            }
            RecoveryStep::ReverifyEligibility => match self.reverify_current_channel().await {
                Ok(true) => {
                    tracing::info!("Twitch still says this channel progresses our drops");
                    Ok(RecoveryOutcome::default())
                }
                Ok(false) => {
                    tracing::warn!("Twitch says this channel progresses nothing; leaving");
                    self.quarantine_current(now);
                    Ok(RecoveryOutcome {
                        resync: true,
                        ..Default::default()
                    })
                }
                Err(e) => {
                    tracing::warn!(error = %e, "re-verification failed");
                    Ok(RecoveryOutcome::default())
                }
            },
            RecoveryStep::SwitchChannel => {
                self.quarantine_current(now);
                Ok(RecoveryOutcome {
                    resync: true,
                    ..Default::default()
                })
            }
            RecoveryStep::ResyncCampaigns => {
                self.sync_forced = true;
                Ok(RecoveryOutcome {
                    resync: true,
                    ..Default::default()
                })
            }
            // The engine holds no credentials, so the caller does this one.
            RecoveryStep::ValidateAuth => Ok(RecoveryOutcome {
                validate_auth: true,
                ..Default::default()
            }),
            RecoveryStep::QuarantineCampaign => {
                self.quarantine_current_campaign(now);
                Ok(RecoveryOutcome {
                    resync: true,
                    ..Default::default()
                })
            }
        }
    }

    /// Rung 7: stop trying this campaign for a while.
    ///
    /// Only the primary campaign is rested. A channel advancing several may
    /// have failed for a reason specific to it, and resting every campaign it
    /// touched would punish campaigns that are probably fine elsewhere — the
    /// channel quarantine below already stops us returning here.
    pub fn quarantine_current_campaign(&mut self, now: DateTime<Utc>) {
        if let Some(campaign) = self.current.as_ref().and_then(|w| w.primary_campaign()) {
            let id = campaign.0.clone();
            let name = self.campaign_name(campaign);
            let until = self.quarantine.punish(&format!("campaign:{id}"), now);
            tracing::error!(
                campaign = %id,
                "recovery exhausted; quarantining this campaign and moving on"
            );
            self.emit(now, EventKind::Quarantined { what: name, until });
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

    pub fn current(&self) -> Option<&CurrentWatch> {
        self.current.as_ref()
    }

    pub fn current_channel(&self) -> Option<&Channel> {
        self.current.as_ref().map(|w| &w.channel)
    }

    pub fn drain_events(&mut self) -> Vec<MinerEvent> {
        std::mem::take(&mut self.events)
    }

    /// How long since this channel last proved it is earning what it was
    /// chosen for.
    ///
    /// This is the health clock, not the display clock. See
    /// [`Engine::counts_towards_health`] for why they differ.
    pub fn since_last_confirmation(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.last_health_advance
            .map(|t| now - t)
            .or_else(|| self.progress.since_last_confirmation(now))
    }

    /// Whether a confirmation proves the *current channel* is doing its job.
    ///
    /// A channel chosen for campaign P often also advances Q. Twitch reports
    /// drop progress per account, not per session, so if the user is watching
    /// a different stream in a browser and that advances Q, accepting it here
    /// would reset the stall clock while P silently earned nothing on this
    /// channel — a narrower recurrence of the account-wide progress bug this
    /// project exists to prevent.
    ///
    /// Health is therefore judged on the campaign that drove the choice.
    /// Once that campaign has nothing left to earn, any of the session's
    /// campaigns will do, so finishing the primary does not read as a stall.
    fn counts_towards_health(&self, drop: &DropId) -> bool {
        let Some(watch) = &self.current else {
            return false;
        };
        let Some(primary) = watch.primary_campaign() else {
            return true;
        };
        let Some(campaign) = self.campaigns.iter().find(|c| &c.id == primary) else {
            return true;
        };
        if eligibility::mineable_drops(campaign).is_empty() {
            return true;
        }
        campaign.drops.iter().any(|d| &d.id == drop)
    }

    pub fn state(&self) -> MiningState {
        if self.paused {
            return MiningState::Paused;
        }
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

    /// Leaves the current channel. Progress there tells us nothing about the
    /// next, so everything session-scoped is reset.
    pub fn stop_watching(&mut self, now: DateTime<Utc>, reason: StopReason) {
        if let Some(w) = &self.current {
            let channel = w.channel.id.clone();
            self.emit(now, EventKind::StoppedWatching { channel, reason });
        }
        self.current = None;
        self.progress.reset();
        self.watchdog.reset();
        self.recovery_step = None;
        self.last_health_advance = None;
        self.last_watch = None;
        self.last_poll = None;
    }

    /// Kept for callers that leave because of an error or an offline stream.
    pub fn switch_away(&mut self, now: DateTime<Utc>) {
        self.stop_watching(now, StopReason::Error);
    }

    fn emit(&mut self, at: DateTime<Utc>, kind: EventKind) {
        self.events.push(MinerEvent { at, kind });
    }

    // -----------------------------------------------------------------------
    // Sync
    // -----------------------------------------------------------------------

    /// Whether the choice of channel should be reconsidered.
    ///
    /// Not only when idle. Selection used to run exclusively when nothing was
    /// being watched, which meant the strategy was consulted once and then
    /// never again for the life of a stream: a campaign expiring tonight could
    /// not displace a comfortable one already in progress. Hysteresis in
    /// [`crate::strategy::choose`] is what stops the periodic re-evaluation
    /// from turning into thrash.
    pub fn needs_selection(&self, now: DateTime<Utc>) -> bool {
        if self.paused {
            return false;
        }
        self.current.is_none()
            || self.reselect_forced
            || self
                .last_selection
                .is_none_or(|t| now - t >= self.config.reselect_interval)
    }

    /// Whether campaigns are due to be re-fetched.
    pub fn needs_sync(&self, now: DateTime<Utc>) -> bool {
        self.sync_forced
            || self
                .last_sync
                .is_none_or(|t| now - t >= self.config.sync_interval)
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
        let mut unlinked: Vec<RejectedView> = Vec::new();
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
            //
            // It is still recorded as rejected: "113 campaigns you could mine
            // if you linked those accounts" is exactly what the dashboard
            // should be telling you, and dropping them silently would make the
            // totals claim the campaigns never existed.
            if !campaign.is_account_linked {
                skipped_unlinked += 1;
                unlinked.push(rejected_view(&campaign, RejectReason::NotLinked));
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
        let mut recently_ended = Vec::new();
        for campaign in merged.into_values() {
            match eligibility::assess(&campaign, now) {
                Ok(()) => eligible.push(campaign),
                Err(reason) => {
                    // A campaign that ended in the last day may still hold
                    // drops we earned and never claimed.
                    if reason == RejectReason::Expired
                        && now < campaign.ends_at + CLAIM_GRACE
                        && campaign.drops.iter().any(|d| d.is_claimable())
                    {
                        recently_ended.push(campaign.clone());
                    }
                    rejected.push(rejected_view(&campaign, reason));
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
        rejected.extend(unlinked);
        // Deterministic order so the dashboard does not reshuffle on refresh.
        rejected.sort_by(|a, b| a.reason.cmp(&b.reason).then(a.name.cmp(&b.name)));
        self.rejected = rejected;

        // Deterministic order: soonest deadline first, so behaviour is
        // reproducible rather than dependent on hash iteration order.
        eligible.sort_by(|a, b| a.ends_at.cmp(&b.ends_at).then(a.id.cmp(&b.id)));

        // Report everything Twitch offered, not just what was expanded — the
        // unlinked majority is real, and omitting it understates the picture.
        self.emit(
            now,
            EventKind::CampaignsSynced {
                total: total + skipped_unlinked,
                eligible: eligible.len(),
            },
        );
        self.recently_ended = recently_ended;
        self.campaigns = eligible;
        self.last_sync = Some(now);
        self.sync_forced = false;
        // Deadlines and remaining minutes have just changed, so the previous
        // choice may no longer be the right one.
        self.reselect_forced = true;

        // A campaign we are mining may have vanished or been completed under
        // us. Continuing to report it as the current session would be a lie.
        if let Some(w) = &mut self.current {
            w.campaigns
                .retain(|id| self.campaigns.iter().any(|c| &c.id == id));
            if w.campaigns.is_empty() {
                tracing::info!("every campaign this channel was advancing is gone; reselecting");
                self.stop_watching(now, StopReason::NotEarning);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Selection
    // -----------------------------------------------------------------------

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
    ) -> Result<Option<CurrentWatch>, ApiError> {
        if self.paused {
            return Ok(None);
        }
        self.quarantine.prune(now);
        // Mark the pass handled only once it has actually happened. Clearing
        // the flag first meant one flaky directory request threw away the
        // user's command: `select_channel` returned Err, the caller logged it,
        // and `needs_selection` then reported nothing was due for another ten
        // minutes — with the dashboard already told the command succeeded.
        let (candidates, fallbacks) = self.gather_candidates(now).await?;
        self.last_selection = Some(now);
        self.reselect_forced = false;
        // Kept for the dashboard, so the user sees the choice we faced.
        self.live = candidates.clone();

        // An explicit "watch this one" beats the strategy outright.
        if let Some(login) = self.forced_channel.take() {
            match candidates
                .iter()
                .chain(fallbacks.iter())
                .find(|c| c.channel.login.eq_ignore_ascii_case(&login))
            {
                Some(c) => {
                    let reason = format!("you asked to watch {}", c.channel.display_name);
                    self.begin_watching(now, c.clone(), reason);
                    return Ok(self.current.clone());
                }
                None => {
                    let message = format!(
                        "{login} is not live, or is not streaming anything you can earn drops for"
                    );
                    tracing::warn!(login, "requested channel is unavailable");
                    self.emit(
                        now,
                        EventKind::Notice {
                            message,
                            severity: Severity::Warn,
                        },
                    );
                }
            }
        }

        let current_id = self.current.as_ref().map(|w| w.channel.id.clone());
        let (decision, reason) = {
            let world = WorldView {
                now,
                campaigns: &self.campaigns,
                candidates: &candidates,
                favourites: &self.favourites,
                priority_games: &self.priority_games,
                pinned: self.pinned.as_deref(),
                current: current_id.as_ref(),
            };
            let decision = strategy::choose(self.strategy.as_ref(), &world);
            let reason = match &decision {
                Decision::Watch(c) => strategy::explain(c, &world, self.strategy.as_ref()),
                Decision::Idle(_) => String::new(),
            };
            (decision, reason)
        };

        match decision {
            Decision::Watch(c) => {
                // Re-choosing the channel we are already on must not restart
                // the session: `begin_watching` resets the progress tracker and
                // the stall clock, so doing it here would discard confirmed
                // progress every time the choice was reconsidered — and make a
                // healthy session look like it had only just started.
                if self
                    .current
                    .as_ref()
                    .is_some_and(|w| w.channel.id == c.channel.id)
                {
                    if let Some(w) = &mut self.current {
                        w.campaigns = c.campaigns().cloned().collect();
                        w.drops = c.drops().cloned().collect();
                        w.stream = c.stream.clone();
                        w.reason = reason;
                    }
                    return Ok(self.current.clone());
                }
                self.begin_watching(now, *c, reason);
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
            let reason = format!(
                "unverified fallback: nobody passed the drop check, so {} was tried \
                 on the directory's drops-enabled tag alone",
                c.channel.display_name
            );
            self.begin_watching(now, c, reason);
            return Ok(self.current.clone());
        }

        Ok(None)
    }

    /// What we want from one campaign, resolved once per selection pass.
    fn wants(&mut self, now: DateTime<Utc>) -> Vec<Want> {
        let mut wants = Vec::new();
        let mut excluded = Vec::new();

        for campaign in &self.campaigns {
            if self.excluded_campaigns.contains(&campaign.id.0)
                || self.excluded_games.contains(&campaign.game.name.to_lowercase())
            {
                continue;
            }
            // A campaign that exhausted the recovery ladder is rested, not
            // retried immediately.
            if self
                .quarantine
                .is_quarantined(&format!("campaign:{}", campaign.id.0), now)
            {
                continue;
            }
            // Do not spend requests on a campaign that cannot finish anyway.
            if !strategy::is_feasible(campaign, now) && self.pinned.is_none() {
                excluded.push(campaign.id.clone());
                continue;
            }

            let drops: HashSet<DropId> = eligibility::mineable_drops(campaign)
                .into_iter()
                .map(|d| d.id.clone())
                .collect();
            if drops.is_empty() {
                continue;
            }

            wants.push(Want {
                campaign: campaign.id.clone(),
                game: campaign.game.clone(),
                slack: strategy::slack(campaign, now),
                allowed: campaign
                    .allowed_channels
                    .iter()
                    .map(|c| c.id.clone())
                    .collect(),
                allowed_channels: campaign.allowed_channels.clone(),
                drops,
            });
        }

        for campaign in excluded {
            self.emit(
                now,
                EventKind::CampaignRejected {
                    campaign,
                    reason: RejectReason::Infeasible,
                },
            );
        }
        wants
    }

    /// Builds the candidate set the strategy chooses from.
    ///
    /// Returns `(verified, fallbacks)`: candidates Twitch confirmed can
    /// progress our drops, and drops-enabled channels that failed or were
    /// excluded by that check.
    ///
    /// Requests are deduplicated twice over. The channel directory is queried
    /// once per *game* rather than once per campaign, and the availability
    /// check runs once per *channel* rather than once per channel-campaign
    /// pair. The previous version did neither, so with 17 eligible campaigns
    /// across a handful of games it re-asked about the same streamers a dozen
    /// times each and exhausted its request budget long before it had looked
    /// at everything on offer.
    async fn gather_candidates(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<(Vec<Candidate>, Vec<Candidate>), ApiError> {
        let wants = self.wants(now);
        if wants.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        // --- one directory query per distinct game --------------------------
        //
        // A campaign restricted to specific channels needs a deeper page:
        // allow-listed streamers are typically small, and the default page of
        // most-relevant channels does not contain them.
        let mut games: Vec<(Game, u32)> = Vec::new();
        for w in &wants {
            let limit = if w.allowed.is_empty() {
                self.config.channel_limit
            } else {
                self.config.allow_list_channel_limit
            };
            match games.iter_mut().find(|(g, _)| g.id == w.game.id) {
                Some((_, existing)) => *existing = (*existing).max(limit),
                None => games.push((w.game.clone(), limit)),
            }
        }

        let mut channels_by_game: HashMap<GameId, Vec<(Channel, Stream)>> = HashMap::new();
        for (game, limit) in games {
            let found = self.api.live_channels(&game, limit).await?;
            channels_by_game.insert(game.id.clone(), found);
        }

        // --- resolve short allow-lists directly -----------------------------
        //
        // A campaign locked to one small streamer is invisible to the
        // directory: they will not be on any page of it while they have a
        // handful of viewers. Asking about them by name is the only way to
        // find them, and these are exactly the exclusive campaigns users care
        // most about.
        let direct: Vec<(usize, Channel)> = wants
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                !w.allowed.is_empty() && w.allowed.len() <= self.config.max_direct_allow_list_lookups
            })
            .flat_map(|(i, w)| {
                let known: HashSet<&ChannelId> = channels_by_game
                    .get(&w.game.id)
                    .map(|list| list.iter().map(|(c, _)| &c.id).collect())
                    .unwrap_or_default();
                w.allowed_channels
                    .iter()
                    .filter(|c| !known.contains(&c.id))
                    .filter(|c| !self.quarantine.is_quarantined(&c.id.0, now))
                    .map(move |c| (i, c.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();

        if !direct.is_empty() {
            let resolved: Vec<(usize, Channel, Option<Stream>)> =
                futures_util::stream::iter(direct.into_iter().map(|(i, channel)| {
                    let api = &self.api;
                    async move {
                        let stream = api.stream_info(&channel.login).await.unwrap_or_else(|e| {
                            tracing::debug!(
                                channel = %channel.login,
                                error = %e,
                                "allow-listed channel lookup failed"
                            );
                            None
                        });
                        (i, channel, stream)
                    }
                }))
                .buffer_unordered(self.config.check_concurrency.max(1))
                .collect()
                .await;

            for (i, channel, stream) in resolved {
                let Some(stream) = stream else { continue };
                tracing::debug!(
                    channel = %channel.login,
                    "allow-listed channel found live outside the directory page"
                );
                channels_by_game
                    .entry(wants[i].game.id.clone())
                    .or_default()
                    .push((channel, stream));
            }
        }

        // --- map each channel to every campaign it could serve --------------
        let mut by_channel: BTreeMap<ChannelId, ChannelWork> = BTreeMap::new();
        let mut campaigns_without_channels = Vec::new();

        for (i, want) in wants.iter().enumerate() {
            let Some(channels) = channels_by_game.get(&want.game.id) else {
                continue;
            };
            let mut any = false;
            let mut taken = 0usize;
            for (channel, stream) in channels {
                // An allow-list, when present, is absolute.
                if !want.allowed.is_empty() && !want.allowed.contains(&channel.id) {
                    continue;
                }
                if self.quarantine.is_quarantined(&channel.id.0, now) {
                    continue;
                }
                // The campaign has options; it exists and is streamable, even
                // if we stop gathering more of them below.
                any = true;
                // The directory returns the busiest streams first, and the
                // strategy only ever picks one channel. Verifying every one of
                // the thirty channels streaming a popular game costs requests
                // and buys nothing.
                if taken >= self.config.max_channels_per_campaign {
                    break;
                }
                taken += 1;
                by_channel
                    .entry(channel.id.clone())
                    .or_insert_with(|| ChannelWork {
                        channel: channel.clone(),
                        stream: stream.clone(),
                        wants: Vec::new(),
                    })
                    .wants
                    .push(i);
            }
            if !any {
                campaigns_without_channels.push(want.campaign.clone());
            }
        }

        for campaign in campaigns_without_channels {
            self.emit(
                now,
                EventKind::CampaignRejected {
                    campaign,
                    reason: RejectReason::NoEligibleChannel,
                },
            );
        }

        // --- spend the check budget on the most urgent channels first -------
        //
        // The budget used to be spent in campaign-list order, which meant a
        // long tail of relaxed campaigns could consume every check before the
        // one expiring tonight was ever looked at.
        let mut work: Vec<ChannelWork> = by_channel.into_values().collect();
        work.sort_by(|a, b| {
            let urgency = |w: &ChannelWork| {
                w.wants
                    .iter()
                    .map(|i| wants[*i].slack)
                    .min()
                    .unwrap_or(Duration::MAX)
            };
            urgency(a)
                .cmp(&urgency(b))
                .then(b.wants.len().cmp(&a.wants.len()))
                .then(b.stream.viewers.cmp(&a.stream.viewers))
                .then(a.channel.login.cmp(&b.channel.login))
        });

        let budget = self.config.max_availability_checks;
        let skipped = work.len().saturating_sub(budget);
        work.truncate(budget);
        let checks = work.len();

        // Run the checks concurrently.
        //
        // These were sequential, and against a real account 60 of them took 27
        // seconds — during which the miner was watching nothing at all. They
        // are independent read-only requests, so the only reason to serialise
        // them was that nobody had made them concurrent. The cap keeps us from
        // opening a hundred sockets at Twitch in one burst.
        let results: Vec<(ChannelWork, Result<Vec<DropId>, ApiError>)> =
            futures_util::stream::iter(work.into_iter().map(|item| {
                let api = &self.api;
                async move {
                    let outcome = api.available_drops(&item.channel.id).await;
                    (item, outcome)
                }
            }))
            .buffer_unordered(self.config.check_concurrency.max(1))
            .collect()
            .await;

        let mut verified = Vec::new();
        let mut fallbacks = Vec::new();

        for (item, outcome) in results {
            // Everything this channel could serve, if the check comes back
            // inconclusive and we fall back to trusting the directory.
            let hoped: Vec<Advance> = item
                .wants
                .iter()
                .map(|i| Advance {
                    campaign: wants[*i].campaign.clone(),
                    drops: wants[*i].drops.iter().cloned().collect(),
                })
                .collect();

            match outcome {
                Ok(available) => {
                    let available: HashSet<DropId> = available.into_iter().collect();
                    let advances: Vec<Advance> = item
                        .wants
                        .iter()
                        .filter_map(|i| {
                            let want = &wants[*i];
                            let drops: Vec<DropId> = want
                                .drops
                                .iter()
                                .filter(|d| available.contains(d))
                                .cloned()
                                .collect();
                            (!drops.is_empty()).then(|| Advance {
                                campaign: want.campaign.clone(),
                                drops,
                            })
                        })
                        .collect();

                    if !advances.is_empty() {
                        verified.push(make_candidate(&item, advances, &wants));
                    } else if item.stream.drops_enabled {
                        fallbacks.push(make_candidate(&item, hoped, &wants));
                    }
                }
                Err(e) => {
                    // A failed check must not silently exclude a channel.
                    tracing::warn!(
                        channel = %item.channel.login,
                        error = %e,
                        "availability check failed; keeping as a fallback"
                    );
                    if item.stream.drops_enabled {
                        fallbacks.push(make_candidate(&item, hoped, &wants));
                    }
                }
            }
        }

        // Concurrency makes completion order arbitrary; sort so selection is
        // reproducible rather than a race.
        let order = |c: &Candidate| c.channel.login.clone();
        verified.sort_by_key(order);
        fallbacks.sort_by_key(order);

        if skipped > 0 {
            // Silently checking only some of the channels would make "nobody
            // is live for this campaign" indistinguishable from "we ran out of
            // budget before looking".
            tracing::warn!(
                limit = self.config.max_availability_checks,
                skipped,
                "availability-check budget reached; some live channels were not checked"
            );
            self.emit(
                now,
                EventKind::Notice {
                    message: format!(
                        "Checked {checks} channels and ran out of budget with {skipped} \
                         left; the busiest and most urgent were checked first"
                    ),
                    severity: Severity::Info,
                },
            );
        }

        Ok((verified, fallbacks))
    }

    fn begin_watching(&mut self, now: DateTime<Utc>, candidate: Candidate, reason: String) {
        self.progress.reset();
        self.progress.begin(now);
        self.watchdog.reset();
        self.recovery_step = None;
        self.last_health_advance = Some(now);
        self.last_watch = None;
        self.last_poll = None;

        let campaigns: Vec<CampaignId> = candidate.campaigns().cloned().collect();
        let drops: Vec<DropId> = candidate.drops().cloned().collect();
        let primary = campaigns.first().cloned().unwrap_or(CampaignId(String::new()));

        self.current = Some(CurrentWatch {
            channel: candidate.channel.clone(),
            stream: candidate.stream.clone(),
            campaigns: campaigns.clone(),
            drops,
            since: now,
            reason: reason.clone(),
        });

        self.emit(
            now,
            EventKind::NowWatching {
                channel: candidate.channel,
                campaign: primary,
                campaigns: campaigns.len(),
                reason,
            },
        );
    }

    // -----------------------------------------------------------------------
    // Mining
    // -----------------------------------------------------------------------

    /// One iteration: claim anything claimable, send a heartbeat if due, then
    /// poll progress as a backstop to the websocket.
    pub async fn tick(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        // Claiming happens even while paused. A drop that has already been
        // earned is ours; "pause" means stop watching, not abandon what the
        // account already has waiting for it.
        self.claim_ready(now).await?;
        if self.paused {
            return Ok(());
        }

        let Some(watch) = self.current.clone() else {
            return Ok(());
        };

        let watch_due = self
            .last_watch
            .is_none_or(|last| now - last >= self.config.watch_interval);
        if watch_due {
            match self.api.send_watch(&watch.channel, &watch.stream).await {
                Ok(()) => {
                    self.last_watch = Some(now);
                    self.emit(
                        now,
                        EventKind::WatchSent {
                            channel: watch.channel.id.clone(),
                        },
                    );
                }
                Err(e) => {
                    self.emit(
                        now,
                        EventKind::WatchFailed {
                            channel: watch.channel.id.clone(),
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
            if let Some(p) = self.api.current_progress(&watch.channel.id).await? {
                self.on_pushed_progress(p, now);
            }
        }
        Ok(())
    }

    /// Whether a drop belongs to a campaign the current session is advancing.
    ///
    /// Twitch reports drop events per *account*, not per session. If the user
    /// is also watching in a browser, those events arrive here too. Letting
    /// them count would reset the stall clock while this miner earned nothing
    /// — the precise failure this project exists to prevent, so the check is
    /// deliberately strict.
    ///
    /// It is scoped to *every* campaign this channel advances, not just the
    /// primary one: with a channel serving four campaigns, accepting progress
    /// only for the first would discard three quarters of the confirmations
    /// and trip the stall detector on a session that is working perfectly.
    fn is_progress_for_current_session(&self, drop: &DropId) -> bool {
        let Some(watch) = &self.current else {
            return false;
        };
        watch.campaigns.iter().any(|campaign_id| {
            self.campaigns
                .iter()
                .find(|c| &c.id == campaign_id)
                .is_some_and(|c| c.drops.iter().any(|d| &d.id == drop))
        })
    }

    /// Records confirmed progress, whether it arrived by websocket or by poll.
    ///
    /// Progress for anything other than what this session is mining is ignored.
    pub fn on_pushed_progress(&mut self, p: DropProgress, now: DateTime<Utc>) {
        if !self.is_progress_for_current_session(&p.drop_id) {
            tracing::debug!(
                drop = %p.drop_id,
                "ignoring progress for a drop this session is not mining"
            );
            return;
        }
        if !self.progress.record(p.clone(), now) {
            return;
        }
        // Real progress on what we came for ends any stall episode in flight.
        if self.counts_towards_health(&p.drop_id) {
            self.last_health_advance = Some(now);
            self.watchdog.on_progress();
            self.recovery_step = None;
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

    /// Claims a channel-points bonus.
    ///
    /// Only on the channel being watched: claiming elsewhere would mean acting
    /// on a session this miner is not running.
    pub async fn claim_points(
        &mut self,
        channel: &ChannelId,
        claim_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ApiError> {
        if self.current_channel().is_none_or(|c| &c.id != channel) {
            return Ok(());
        }
        self.api.claim_points(channel, claim_id).await?;
        self.emit(
            now,
            EventKind::PointsClaimed {
                channel: channel.clone(),
            },
        );
        Ok(())
    }

    /// Marks a drop as claimable, as reported over the websocket.
    pub fn on_drop_claimable(&mut self, drop_id: &DropId, claim_id: &str) {
        for campaign in &mut self.campaigns {
            if let Some(d) = campaign.drops.iter_mut().find(|d| &d.id == drop_id) {
                d.claim_id = Some(claim_id.to_owned());
            }
        }
    }

    /// Claims everything ready, whether or not it belongs to the current
    /// session. A drop earned an hour ago on a channel we have since left is
    /// still ours — and so is one from a campaign that ended twenty minutes
    /// ago, which Twitch allows for a further twenty-four hours.
    pub async fn claim_ready(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let ready: Vec<(CampaignId, DropId, Option<String>, String)> = self
            .campaigns
            .iter()
            .chain(self.recently_ended.iter())
            .flat_map(|c| c.drops.iter().map(move |d| (c, d)))
            .filter(|(_, d)| d.is_claimable())
            .map(|(c, d)| (c.id.clone(), d.id.clone(), d.claim_id.clone(), d.name.clone()))
            .collect();

        for (campaign_id, drop_id, claim_id, name) in ready {
            // A claim failing must not abort the tick: the other drops are
            // still worth claiming, and the heartbeat still has to be sent.
            if let Err(e) = self
                .api
                .claim_drop(&drop_id, &campaign_id, claim_id.as_deref())
                .await
            {
                tracing::warn!(
                    drop = %drop_id,
                    error = %e,
                    "claim refused; leaving it unclaimed so it is retried"
                );
                self.emit(
                    now,
                    EventKind::Notice {
                        message: format!("Could not claim {name}: {e}. It will be retried."),
                        severity: Severity::Warn,
                    },
                );
                continue;
            }

            // Only now is it claimed. Marking it before the server agreed is
            // how a refused claim became permanent.
            for campaign in self
                .campaigns
                .iter_mut()
                .chain(self.recently_ended.iter_mut())
            {
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

/// How long after a campaign ends its earned drops can still be claimed.
/// Twitch's own documented grace period.
const CLAIM_GRACE: Duration = Duration::hours(24);

/// A campaign's requirements, resolved once per selection pass.
struct Want {
    campaign: CampaignId,
    game: Game,
    /// Spare time before the deadline; drives where the check budget goes.
    slack: Duration,
    allowed: HashSet<ChannelId>,
    /// The allow-listed channels themselves, for direct lookup.
    allowed_channels: Vec<Channel>,
    drops: HashSet<DropId>,
}

/// One channel and every campaign it might serve.
struct ChannelWork {
    channel: Channel,
    stream: Stream,
    /// Indices into the `wants` list.
    wants: Vec<usize>,
}

/// Builds a candidate with its advances ordered most-urgent-first, which is
/// what makes `primary_campaign` meaningful.
fn make_candidate(item: &ChannelWork, mut advances: Vec<Advance>, wants: &[Want]) -> Candidate {
    advances.sort_by(|a, b| {
        let slack = |id: &CampaignId| {
            wants
                .iter()
                .find(|w| &w.campaign == id)
                .map(|w| w.slack)
                .unwrap_or(Duration::MAX)
        };
        slack(&a.campaign)
            .cmp(&slack(&b.campaign))
            .then(a.campaign.cmp(&b.campaign))
    });
    Candidate {
        channel: item.channel.clone(),
        stream: item.stream.clone(),
        advances,
    }
}

fn rejected_view(campaign: &DropCampaign, reason: RejectReason) -> RejectedView {
    RejectedView {
        id: campaign.id.0.clone(),
        name: campaign.name.clone(),
        game: campaign.game.name.clone(),
        reason: reason.label().to_owned(),
        explanation: reason.explain().to_owned(),
        // Only offer the link where it is the actual remedy.
        action_url: match reason {
            RejectReason::NotLinked => campaign.account_link_url.clone(),
            _ => None,
        },
    }
}
