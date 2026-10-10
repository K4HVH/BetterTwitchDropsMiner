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
use crate::strategy::{
    self, Advance, Candidate, Confidence, Decision, MiningStrategy, WorldView,
};
use crate::watchdog::{Quarantine, RecoveryStep, Verdict, Watchdog, WatchdogConfig};
use futures_util::StreamExt;
use chrono::{DateTime, Duration, Utc};

/// Games whose campaigns are earned on *any* channel, not only ones streaming
/// that game. Twitch uses these for event drops — a tournament broadcast is
/// categorised as the event, not as the game the drops belong to, so matching
/// on the campaign's own game finds nobody and the campaign looks dead.
const ANY_CHANNEL_GAME_IDS: [&str; 2] = ["509663", "509672"];

/// The longest a user-supplied name may be: a channel login, a game name, a
/// campaign name or id. Twitch's longest are far shorter; this is a bound on
/// abuse and accidents, not a validation rule.
const MAX_ENTRY_CHARS: usize = 200;
/// How many entries any one user list may hold. Everything in these lists is
/// written to `config.toml` and repeated in every pushed snapshot.
const MAX_LIST_ENTRIES: usize = 200;
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
    /// The same, but while something is close to being lost.
    ///
    /// Deadline pressure is the only time a ten-minute wait actually costs
    /// anything: a campaign with days of slack does not care when it is
    /// picked up, and one with an hour of slack very much does. Reacting
    /// faster exactly then is cheaper and more targeted than subscribing to
    /// every channel's stream state in the hope of noticing sooner.
    pub urgent_reselect_interval: Duration,
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
    /// A ceiling on those lookups across the whole pass. Without it, the
    /// per-campaign limit is not a limit: twenty qualifying campaigns would
    /// each spend their own allowance.
    pub max_direct_lookups_per_pass: usize,
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
            urgent_reselect_interval: Duration::minutes(2),
            channel_limit: 30,
            max_detail_fetches: 60,
            max_availability_checks: 160,
            check_concurrency: 8,
            max_channels_per_campaign: 8,
            allow_list_channel_limit: 100,
            max_direct_allow_list_lookups: 8,
            max_direct_lookups_per_pass: 24,
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
    /// How the choice was established. Kept so the dashboard can say whether
    /// Twitch named these drops or the miner inferred them from the
    /// directory — a distinction the user cannot otherwise see.
    pub confidence: Confidence,
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
    /// Campaigns that have not started yet but will soon, with when. Checked
    /// each pass so mining begins the moment one opens rather than at the next
    /// scheduled sync — launch day is when people care most.
    upcoming: Vec<(CampaignId, DateTime<Utc>)>,
    /// Campaigns that have ended but whose earned drops can still be claimed.
    /// Twitch allows claiming for 24 hours after a campaign closes; dropping
    /// them at the moment they expired threw away rewards already worked for.
    /// Campaigns that will never be mined again but may still hold drops that
    /// are earned and unclaimed: expired inside Twitch's 24-hour claim window,
    /// or subscription rewards the user genuinely subscribed for.
    claim_only: Vec<DropCampaign>,
    /// Set when recovery itself left the channel, so the next selection
    /// continues the episode instead of starting a fresh one.
    carry_recovery: bool,
    /// Why each unavailable campaign was rejected, for the dashboard.
    rejected: Vec<RejectedView>,
    current: Option<CurrentWatch>,
    /// The candidate set from the last selection pass, so the dashboard can
    /// show what was actually on offer rather than only what won.
    live: Vec<Candidate>,
    /// How many live channels the last selection pass found per campaign, by
    /// campaign id. A campaign absent from the map was not looked at, which
    /// the dashboard reports differently from a campaign nobody is streaming.
    live_counts: HashMap<String, usize>,
    /// Why the last selection pass could not finish, if it could not.
    ///
    /// "Idle because nothing is worth watching" and "idle because the question
    /// could not be asked" are different facts, and only one of them is about
    /// Twitch's streamers. Reporting the first when the second is true is how
    /// a miner sidelined by three failed requests came to tell its user that
    /// nobody was live. Cleared by any pass that completes, so it can never
    /// age into a claim about now.
    last_selection_error: Option<String>,
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
    /// Drops whose claim Twitch has refused, and when to try again. Without
    /// this, an `INELIGIBLE` verdict was re-sent on every tick forever.
    claim_backoff: HashMap<DropId, (DateTime<Utc>, u32)>,
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
        // Deduplicated on the way in as well as on the way through a command:
        // a hand-edited config.toml is the one remaining way a repeat could
        // reach the list, and the miner ranks by first match, so a second copy
        // is invisible to it while being visible — and individually
        // clickable — on the dashboard. Every control there addresses an entry
        // by its text, so two identical entries make those controls ambiguous.
        let priority_games = {
            let mut seen = HashSet::new();
            config
                .priority_games
                .iter()
                .map(|g| g.trim().to_lowercase())
                .filter(|g| !g.is_empty())
                .filter(|g| seen.insert(g.clone()))
                .collect()
        };
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
            upcoming: Vec::new(),
            claim_only: Vec::new(),
            carry_recovery: false,
            rejected: Vec::new(),
            current: None,
            live: Vec::new(),
            live_counts: HashMap::new(),
            last_selection_error: None,
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
            claim_backoff: HashMap::new(),
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
    ///
    /// `CommandEffect::reselect` and `self.reselect_forced` say the same
    /// thing to two different readers — the caller, and the next
    /// `needs_selection`. Every arm used to have to remember to set both, and
    /// the three priority-list commands set only the first: they reported
    /// "reselect" to `run.rs`, which broke out of its wait loop, asked
    /// `needs_selection`, was told no, and went back to sleep for ten minutes.
    /// Reordering the priority list did nothing until the timer expired.
    ///
    /// Deriving one from the other here means an arm can only say it wants a
    /// reselection by actually getting one.
    pub fn apply_command(&mut self, cmd: Command, now: DateTime<Utc>) -> CommandEffect {
        let effect = self.apply_command_inner(cmd, now);
        if effect.reselect {
            self.reselect_forced = true;
        }
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

    /// Normalises a name the user supplied, or explains why it is unusable.
    ///
    /// Everything these lists hold is persisted to `config.toml` and repeated
    /// in every snapshot the dashboard pushes. Unbounded, one paste accident
    /// is permanent: a 200 KB entry was accepted, written to the config file,
    /// and then carried in every SSE frame for the life of the process. The
    /// longest thing that legitimately goes in here is a campaign name, and
    /// Twitch's are nowhere near this limit.
    fn clean_entry(value: &str, what: &str) -> Result<String, CommandEffect> {
        let cleaned = value.trim().to_lowercase();
        if cleaned.is_empty() {
            return Err(CommandEffect::message(format!("A {what} is required")));
        }
        if cleaned.chars().count() > MAX_ENTRY_CHARS {
            return Err(CommandEffect::message(format!(
                "That {what} is {} characters; the limit is {MAX_ENTRY_CHARS}",
                cleaned.chars().count()
            )));
        }
        Ok(cleaned)
    }

    fn apply_command_inner(&mut self, cmd: Command, now: DateTime<Utc>) -> CommandEffect {
        match cmd {
            Command::Pause => {
                if self.paused {
                    return CommandEffect::message("Already paused");
                }
                self.paused = true;
                self.stop_watching(now, StopReason::UserRequest);
                // A paused miner runs no selection pass, so these counts stop
                // being refreshed. Keeping them would age silently into a claim
                // about who is live now; dropping them makes the dashboard say
                // it has not looked, which is the truth.
                self.live.clear();
                self.live_counts.clear();
                self.last_selection_error = None;
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
                let login = match channel.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                    Some(c) => match Self::clean_entry(c, "channel name") {
                        Ok(c) => Some(c),
                        Err(e) => return e,
                    },
                    None => None,
                };
                let msg = match &login {
                    Some(c) => format!("Pinned {c}"),
                    None => "Pin cleared".to_string(),
                };
                self.pinned = login;
                self.reselect_forced = true;
                CommandEffect::message(msg).with_persist().with_reselect()
            }
            Command::SetFavourite { login, on } => {
                let key = match Self::clean_entry(&login, "channel name") {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                if on && self.favourites.len() >= MAX_LIST_ENTRIES {
                    return CommandEffect::message(format!(
                        "The favourites list is full at {MAX_LIST_ENTRIES} channels"
                    ));
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
                if games.len() > MAX_LIST_ENTRIES {
                    return CommandEffect::message(format!(
                        "That is {} entries; the limit is {MAX_LIST_ENTRIES}",
                        games.len()
                    ));
                }
                let mut cleaned = Vec::with_capacity(games.len());
                for g in &games {
                    if g.trim().is_empty() {
                        continue;
                    }
                    match Self::clean_entry(g, "priority entry") {
                        Ok(g) => cleaned.push(g),
                        Err(e) => return e,
                    }
                }
                // Not `dedup`, which only collapses *adjacent* repeats: an
                // alternating list would keep every one of them.
                let mut seen = HashSet::new();
                cleaned.retain(|g| seen.insert(g.clone()));
                self.priority_games = cleaned;
                let n = self.priority_games.len();
                self.reselect_forced = true;
                CommandEffect::message(format!("Priority list set to {n} entries"))
                    .with_persist()
                    .with_reselect()
            }
            Command::AddPriorityEntry { entry } => {
                let entry = match Self::clean_entry(&entry, "priority entry") {
                    Ok(e) => e,
                    Err(e) => return e,
                };
                if self.priority_games.len() >= MAX_LIST_ENTRIES {
                    return CommandEffect::message(format!(
                        "The priority list is full at {MAX_LIST_ENTRIES} entries"
                    ));
                }
                if self.priority_games.contains(&entry) {
                    return CommandEffect::message(format!("'{entry}' is already prioritised"));
                }
                self.priority_games.push(entry.clone());
                CommandEffect::message(format!(
                    "Prioritised '{entry}' at position {}",
                    self.priority_games.len()
                ))
                .with_persist()
                .with_reselect()
            }
            Command::RemovePriorityEntry { entry } => {
                let entry = entry.trim().to_lowercase();
                let before = self.priority_games.len();
                self.priority_games.retain(|g| g != &entry);
                if self.priority_games.len() == before {
                    return CommandEffect::message(format!("'{entry}' was not prioritised"));
                }
                CommandEffect::message(format!("Removed '{entry}' from the priority list"))
                    .with_persist()
                    .with_reselect()
            }
            Command::MovePriorityEntry { entry, up } => {
                let entry = entry.trim().to_lowercase();
                let Some(i) = self.priority_games.iter().position(|g| g == &entry) else {
                    return CommandEffect::message(format!("'{entry}' was not prioritised"));
                };
                let j = if up {
                    match i.checked_sub(1) {
                        Some(j) => j,
                        // Already first: not an error, just nothing to do.
                        None => return CommandEffect::message(format!("'{entry}' is already first")),
                    }
                } else if i + 1 < self.priority_games.len() {
                    i + 1
                } else {
                    return CommandEffect::message(format!("'{entry}' is already last"));
                };
                self.priority_games.swap(i, j);
                CommandEffect::message(format!("Moved '{entry}' to position {}", j + 1))
                    .with_persist()
                    .with_reselect()
            }
            Command::SetCampaignExcluded { campaign, excluded } => {
                // Campaign ids are Twitch UUIDs. Anything longer is not one,
                // and would be persisted and republished forever.
                if campaign.0.trim().is_empty() || campaign.0.chars().count() > MAX_ENTRY_CHARS {
                    return CommandEffect::message("That is not a campaign id");
                }
                if excluded && self.excluded_campaigns.len() >= MAX_LIST_ENTRIES {
                    return CommandEffect::message(format!(
                        "The excluded-campaigns list is full at {MAX_LIST_ENTRIES}"
                    ));
                }
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
                let key = match Self::clean_entry(&game, "game name") {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                if excluded && self.excluded_games.len() >= MAX_LIST_ENTRIES {
                    return CommandEffect::message(format!(
                        "The excluded-games list is full at {MAX_LIST_ENTRIES}"
                    ));
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
                    format!("Excluded every campaign for {key}")
                } else {
                    format!("Re-included {key}")
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
                confirmed_by_twitch: w.confidence == Confidence::Confirmed,
            }
        });

        // Taken from the selection pass rather than counted off `self.live`,
        // which holds only the channels that survived the per-campaign cap.
        // Counting those reported "8 channels live" for a game with thirty,
        // and — because a campaign the pass skipped has no candidates at all —
        // reported "nobody live" for campaigns nobody had looked at.
        let live_by_campaign = &self.live_counts;

        let ctx = ViewContext {
            now,
            excluded_campaigns: &self.excluded_campaigns,
            excluded_games: &self.excluded_games,
            current_campaigns,
            active_drop: active.map(|(id, _)| id),
            live_by_campaign,
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
                confirmed_by_twitch: c.confidence == Confidence::Confirmed,
            })
            .collect();

        let totals = Totals {
            campaigns_known: self.campaigns.len() + self.rejected.len(),
            campaigns_eligible: self.campaigns.len(),
            campaigns_at_risk: eligible.iter().filter(|c| c.at_risk && !c.excluded).count(),
            // Claimed anywhere counts, including in a campaign that has since
            // expired — it was still earned.
            drops_claimed: self.all_drops().filter(|d| d.is_claimed).count(),
            // Remaining *work*, so only campaigns still being mined, and only
            // tiers watching can finish. A subscription tier is not work, and
            // neither is an unfinished tier in a campaign that has ended.
            drops_remaining: self
                .campaigns
                .iter()
                .flat_map(|c| &c.drops)
                .filter(|d| !d.is_claimed && d.is_earnable_by_watching())
                .count(),
            // Waiting to be claimed, wherever it sits. This is the one total
            // that must see `claim_only`, or the page reports nothing to claim
            // while the miner is claiming it.
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
            last_selection_error: self.last_selection_error.clone(),
            last_sync: self.last_sync,
            next_sync_in_seconds: self.last_sync.map(|t| {
                let scheduled = (t + self.config.sync_interval - now).num_seconds().max(0);
                // A campaign opening sooner than the next scheduled sync is
                // what will actually wake it, so that is the honest countdown.
                self.upcoming
                    .first()
                    .map(|(_, starts)| (*starts - now).num_seconds().max(0))
                    .map_or(scheduled, |opens| scheduled.min(opens))
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

    /// Every drop the miner is responsible for, including those in campaigns
    /// it will never mine but must still claim from.
    fn all_drops(&self) -> impl Iterator<Item = &TimedDrop> {
        self.campaigns
            .iter()
            .chain(self.claim_only.iter())
            .flat_map(|c| &c.drops)
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
    /// The broadcast ID changes when a stream restarts. While spade earned
    /// drop time, heartbeats carrying a stale one were accepted and earned
    /// nothing; now a new ID makes the next heartbeat start watching the new
    /// broadcast afresh. Returns whether the ID actually changed.
    /// Whether a stream still serves any of the campaigns it was chosen for.
    ///
    /// A campaign in one of the sitewide categories is earned on any channel,
    /// so those never fail this. Neither does a stream whose category Twitch
    /// declines to report — absence of an answer is not a wrong answer.
    fn stream_still_serves(&self, stream: &Stream, campaigns: &[CampaignId]) -> bool {
        let Some(playing) = stream.game.as_ref().map(|g| &g.id) else {
            return true;
        };
        campaigns.iter().any(|id| {
            self.campaigns.iter().find(|c| &c.id == id).is_none_or(|c| {
                ANY_CHANNEL_GAME_IDS.contains(&c.game.id.0.as_str()) || &c.game.id == playing
            })
        })
    }

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
                        "the stream restarted; watching the new broadcast afresh"
                    );
                    self.progress.begin(now);
                    self.last_health_advance = Some(now);
                }
                // A streamer who switches category mid-stream stops earning
                // the campaign's drops at once, and nothing else notices: the
                // stream is still live, so the offline check passes, and the
                // per-channel availability check no longer condemns anything.
                // Observed live — a Madden streamer moved to Just Chatting and
                // the miner sat on them, stalled, until the ladder reached the
                // rung that switches channel four minutes later.
                //
                // Unlike that check, this is proof: Twitch grants drops for
                // watching the game, and this channel is demonstrably not
                // streaming it.
                if !self.stream_still_serves(&fresh, &watch.campaigns) {
                    tracing::info!(
                        channel = %watch.channel.login,
                        now_playing = fresh.game.as_ref().map(|g| g.name.as_str()).unwrap_or("nothing"),
                        "channel changed category away from what we are mining"
                    );
                    self.stop_watching(now, StopReason::NotEarning);
                    return Ok(true);
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
            // Confirmation is still worth having: if Twitch names one of our
            // drops, the channel is definitely fine and the stall is
            // elsewhere. The absence of one proves nothing, though — this
            // endpoint stopped listing game campaigns altogether, so an empty
            // intersection is the *normal* answer for a channel that is
            // earning perfectly well. See [`strategy::Confidence`].
            //
            // Treating it as proof made this rung quarantine whatever it was
            // watching roughly eight minutes into any stall, move to the next
            // channel, stall again, and work through the entire directory
            // until every channel for the game was rested for two hours.
            Ok(available) => Ok(available.iter().any(|d| wanted.contains(d))),
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
                // A heartbeat repeated for the same stream is answered from
                // what the transport has already seen, so on its own this
                // retried nothing. Stopping first makes the next one start
                // watching afresh.
                self.api.stop_watch();
                self.force_next_watch();
                Ok(RecoveryOutcome::default())
            }
            RecoveryStep::RefreshStreamInfo => {
                match self.refresh_stream_info(now).await {
                    // The broadcast id moved: watch the new one now rather
                    // than at the next scheduled heartbeat.
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
                // Not "Twitch says this channel earns nothing" — Twitch no
                // longer says that about anything. This rung can confirm a
                // channel and can no longer condemn one, so an unconfirmed
                // answer moves to the next rung, which switches channel
                // anyway. Quarantining here punished a channel for a stall it
                // had not been shown to cause, and did it to every channel in
                // turn.
                Ok(false) => {
                    tracing::debug!(
                        "Twitch did not confirm this channel; escalating rather than resting it"
                    );
                    Ok(RecoveryOutcome::default())
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

    /// Channels whose stream state is worth following, best first.
    ///
    /// The channel being watched always comes first — losing that one is the
    /// urgent case. After it come the candidates the strategy would consider
    /// next, so a better channel coming online is noticed within seconds
    /// rather than at the next scheduled re-evaluation.
    pub fn channels_worth_following(&self, limit: usize) -> Vec<ChannelId> {
        let mut out: Vec<ChannelId> = Vec::with_capacity(limit);
        if let Some(w) = &self.current {
            out.push(w.channel.id.clone());
        }

        let mut rest: Vec<&Candidate> = self
            .live
            .iter()
            .filter(|c| !out.contains(&c.channel.id))
            .collect();
        // Most campaigns advanced, then most drops, then busiest — the same
        // order the strategy would broadly favour.
        rest.sort_by(|a, b| {
            b.campaign_count()
                .cmp(&a.campaign_count())
                .then(b.drop_count().cmp(&a.drop_count()))
                .then(b.stream.viewers.cmp(&a.stream.viewers))
                .then(a.channel.login.cmp(&b.channel.login))
        });

        for candidate in rest {
            if out.len() >= limit {
                break;
            }
            out.push(candidate.channel.id.clone());
        }
        out
    }

    /// Asks for the choice to be reconsidered on the next pass.
    ///
    /// Used when a channel we were following comes online: it may be a better
    /// option than whatever is being watched, and waiting out the re-selection
    /// timer would miss it.
    pub fn request_reselect(&mut self) {
        self.reselect_forced = true;
    }

    /// Drops a channel that has gone offline from the candidate view, so the
    /// dashboard does not keep offering a stream that has ended.
    pub fn on_channel_offline(&mut self, channel: &ChannelId) {
        self.live.retain(|c| &c.channel.id != channel);
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

    /// Whether watching the current channel can still advance anything.
    ///
    /// A drop that is fully earned but not yet claimed is *not* something more
    /// watching can help with, so it deliberately does not count here — even
    /// though [`eligibility::mineable_drops`] still lists it, which it must, or
    /// the campaign would fall out of `self.campaigns` and the drop would never
    /// be claimed at all.
    fn current_can_still_earn(&self) -> bool {
        let Some(watch) = &self.current else {
            return false;
        };
        watch.campaigns.iter().any(|id| {
            self.campaigns.iter().find(|c| &c.id == id).is_some_and(|c| {
                eligibility::mineable_drops(c)
                    .iter()
                    .any(|d| d.remaining_minutes() > 0)
            })
        })
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

    /// Why the last selection pass failed, if it did. `None` means the last
    /// pass finished, so an idle miner really did look and find nothing.
    pub fn selection_failure(&self) -> Option<&str> {
        self.last_selection_error.as_deref()
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
            self.api.stop_watch();
        }
        self.current = None;
        self.progress.reset();
        // Leaving a channel normally starts a fresh episode — the previous
        // channel's failures say nothing about the next one. Leaving *because
        // recovery gave up on it* is the opposite: it is the ladder doing its
        // job, and resetting there meant the ladder could never climb past the
        // rung that switches channel. Resyncing campaigns, revalidating the
        // token and resting the campaign were unreachable, so a campaign whose
        // every channel was failing would switch channels forever instead of
        // escalating to the cause.
        if reason == StopReason::NotEarning {
            // Carry the episode into the next channel: this stop *is* the
            // ladder escalating, and the next channel is the attempt, not a
            // fresh start.
            self.carry_recovery = true;
        } else {
            self.carry_recovery = false;
            self.watchdog.reset();
        }
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
        if self.current.is_none() || self.reselect_forced {
            return true;
        }

        let interval = if self.anything_at_risk(now) {
            self.config.urgent_reselect_interval
        } else {
            self.config.reselect_interval
        };
        self.last_selection.is_none_or(|t| now - t >= interval)
    }

    /// Whether any mineable campaign is close enough to its deadline that a
    /// delay in picking it up could lose it.
    fn anything_at_risk(&self, now: DateTime<Utc>) -> bool {
        self.campaigns
            .iter()
            .filter(|c| !self.excluded_campaigns.contains(&c.id.0))
            .filter(|c| !self.excluded_games.contains(&c.game.name.to_lowercase()))
            .any(|c| {
                let slack = strategy::slack(c, now).num_minutes();
                (0..snapshot::AT_RISK_SLACK_MINUTES).contains(&slack)
            })
    }

    /// Whether campaigns are due to be re-fetched.
    pub fn needs_sync(&self, now: DateTime<Utc>) -> bool {
        // A campaign that has just opened is worth a sync of its own: waiting
        // out the interval would give away the first minutes of a launch, and
        // launch day is exactly when a drop is worth having.
        if self.upcoming.iter().any(|(_, starts)| now >= *starts) {
            return true;
        }
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
        let inventory = self.api.fetch_inventory().await?;
        let awarded = inventory.awarded;
        let mut merged: HashMap<CampaignId, DropCampaign> = inventory
            .campaigns
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let in_progress = merged.len();

        // The dashboard is advisory: if it fails we still mine what we know.
        let mut dashboard_ok = true;
        let offered = match self.api.fetch_campaigns().await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %e, "campaign dashboard unavailable; using inventory only");
                dashboard_ok = false;
                Vec::new()
            }
        };

        let mut fetched = 0usize;
        let mut wanted_details: Vec<CampaignId> = Vec::new();
        let mut skipped_unlinked = 0usize;
        let mut unlinked: Vec<RejectedView> = Vec::new();
        // The listing is the only source of some presentation fields, and the
        // detail response that replaces it does not always carry them. Kept so
        // the richer answer can be reassembled from both.
        let mut listed: HashMap<CampaignId, DropCampaign> = HashMap::new();
        for campaign in offered {
            listed.insert(campaign.id.clone(), campaign.clone());
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
                // `continue`, not `break`: the loop still has to record every
                // remaining campaign in `listed`, which is the only source of
                // artwork for campaigns already in the inventory. Breaking
                // here left everything after the cap with a blank tile,
                // dependent on Twitch's arbitrary listing order.
                continue;
            }
            fetched += 1;
            wanted_details.push(campaign.id.clone());
        }

        // Batched: against a real account this is a couple of dozen campaigns,
        // and asking about them one at a time was most of a sync's wall time.
        // Campaigns Twitch offered that we could not expand. Without a detail
        // response there are no drop tiers, so they can be neither assessed
        // nor rejected: they simply left `merged` and stopped existing, and
        // the totals reported afterwards were short by however many had
        // failed. Sixteen went that way in one sync on 2026-08-27.
        let mut unchecked: Vec<CampaignId> = Vec::new();
        for (id, result) in self.api.campaign_details_many(&wanted_details).await {
            match result {
                Ok(Some(mut full)) => {
                    // The detail response is richer about drops but poorer
                    // about presentation: it frequently returns a game with no
                    // box art where the campaign list had one. Replacing
                    // wholesale therefore *lost* artwork on exactly the
                    // campaigns the user cares about — the mineable ones, which
                    // are the only ones that get expanded. Every rejected
                    // campaign kept its image and over half the eligible ones
                    // showed a blank tile.
                    if let Some(existing) = merged.get(&full.id).or_else(|| listed.get(&full.id)) {
                        full.fill_presentation_from(existing);
                    }
                    merged.insert(full.id.clone(), full);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(campaign = %id, error = %e, "campaign details failed");
                    unchecked.push(id);
                }
            }
        }
        if !unchecked.is_empty() {
            tracing::warn!(
                count = unchecked.len(),
                "campaigns Twitch offered but would not describe; they are not \
                 mineable, not rejected, and not known either way"
            );
        }

        // Campaigns already in the inventory are never detail-fetched — there
        // is nothing to learn — so they never passed through the merge above
        // and kept whatever the inventory knew, which is no artwork at all.
        // That is precisely the set the user has been mining, so those were
        // the blank tiles.
        for (id, campaign) in merged.iter_mut() {
            if let Some(from_list) = listed.get(id) {
                campaign.fill_presentation_from(from_list);
            }
        }

        // Reconcile every campaign against what the account already owns.
        //
        // Applied here rather than at the parse boundary because it has to
        // reach campaigns from all three sources. Twitch removes a campaign
        // from the in-progress inventory once its tiers are claimed, so the
        // fully-claimed ones come back through the dashboard and detail
        // queries — which report them as untouched, zero minutes watched. Those
        // are exactly the campaigns the ledger exists to correct, and exactly
        // the ones it never used to see.
        let before: usize = merged
            .values()
            .flat_map(|c| &c.drops)
            .filter(|d| d.is_claimed)
            .count();
        for campaign in merged.values_mut() {
            campaign.mark_claimed_from_ledger(&awarded);
        }
        let recovered: usize = merged
            .values()
            .flat_map(|c| &c.drops)
            .filter(|d| d.is_claimed)
            .count()
            - before;
        if recovered > 0 {
            tracing::debug!(
                recovered,
                ledger = awarded.len(),
                "tiers already owned, per the awarded-benefit ledger"
            );
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
        let mut claim_only = Vec::new();
        let mut upcoming = Vec::new();
        for campaign in merged.into_values() {
            match eligibility::assess(&campaign, now) {
                Ok(()) => eligible.push(campaign),
                Err(reason) => {
                    // Not mineable is not the same as not *claimable*. Two
                    // rejection reasons leave drops that are already earned
                    // and still ours to take:
                    //
                    // - a campaign that ended within the last day, inside
                    //   Twitch's claim window;
                    // - a subscription reward the user actually subscribed
                    //   for. Watching can never earn it, which is why it is
                    //   rejected, but if Twitch has granted it then refusing
                    //   to claim it loses the user a badge they paid for.
                    // Kept on the strength of an unclaimed drop, not a
                    // currently-claimable one. A subscription reward needs zero
                    // minutes, so it becomes claimable only when Twitch pushes
                    // its claim id — and if the campaign were not being held,
                    // that push would have nowhere to land.
                    let anything_unclaimed = campaign.drops.iter().any(|d| !d.is_claimed);
                    let inside_claim_window = reason == RejectReason::Expired
                        && now < campaign.ends_at + CLAIM_GRACE;
                    let earned_by_subscribing = reason == RejectReason::SubscriptionRequired;
                    if anything_unclaimed && (inside_claim_window || earned_by_subscribing) {
                        claim_only.push(campaign.clone());
                    }
                    // Worth waking for, if it opens soon enough to matter.
                    if reason == RejectReason::NotStarted
                        && campaign.is_account_linked
                        && campaign.starts_at <= now + UPCOMING_HORIZON
                    {
                        upcoming.push((campaign.id.clone(), campaign.starts_at));
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
                total: total + skipped_unlinked + unchecked.len(),
                eligible: eligible.len(),
                unchecked: unchecked.len(),
            },
        );
        // Campaign ids churn every relaunch, so an exclusion list keyed on
        // them grows without bound and stops matching anything. Drop entries
        // for campaigns Twitch no longer offers at all — the dashboard's
        // per-row "hide" button makes accumulating dead ids the normal path.
        //
        // Only ever from a *complete* view. A campaign the user excluded
        // before ever watching it exists only in the dashboard listing, never
        // in the inventory, so pruning after a failed `fetch_campaigns` would
        // decide it had vanished and forget the exclusion — and the miner
        // would then start mining something the user explicitly turned off.
        if dashboard_ok && !self.excluded_campaigns.is_empty() {
            let known: HashSet<&str> = eligible
                .iter()
                .chain(claim_only.iter())
                .map(|c| c.id.0.as_str())
                .chain(self.rejected.iter().map(|r| r.id.as_str()))
                .collect();
            let before = self.excluded_campaigns.len();
            self.excluded_campaigns.retain(|id| known.contains(id.as_str()));
            let dropped = before - self.excluded_campaigns.len();
            if dropped > 0 {
                tracing::debug!(dropped, "forgot exclusions for campaigns Twitch no longer offers");
            }
        }

        upcoming.sort_by_key(|(_, starts)| *starts);
        if let Some((_, next)) = upcoming.first() {
            tracing::debug!(at = %next, "next campaign opens");
        }
        self.upcoming = upcoming;
        self.claim_only = claim_only;
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

    /// Picks the channel the strategy scores highest.
    ///
    /// Candidacy rests on the directory: a channel is offered for a campaign
    /// when it is streaming that campaign's game, passes any allow-list, and
    /// Twitch's drops-enabled filter returned it.
    ///
    /// It is not a veto at all any more. Twitch stopped listing a channel's
    /// game campaigns there, so an answer naming none of our drops is the
    /// normal case for a channel that is earning perfectly well — see
    /// [`Confidence`]. The check can raise confidence and can rescue a channel
    /// carrying no drops-enabled tag; it can never remove one.
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
        let candidates = match self.gather_candidates(now).await {
            Ok(candidates) => candidates,
            Err(e) => {
                // Remembered rather than merely returned. The caller logs it
                // and moves on, and everything downstream then reported plain
                // "Idle" — which the log renders as "no eligible channel" and
                // the dashboard as "no live channel can progress any of your
                // campaigns right now". Both are assertions about Twitch's
                // streamers made by a miner that had just failed to ask.
                self.last_selection_error = Some(e.to_string());
                return Err(e);
            }
        };
        self.last_selection_error = None;
        self.last_selection = Some(now);
        self.reselect_forced = false;
        // Kept for the dashboard, so the user sees the choice we faced. This
        // is every channel considered, not just the ones Twitch confirmed —
        // when it held only confirmed ones it was empty on every real pass,
        // and the dashboard showed nobody live while the miner was mining.
        self.live = candidates.clone();

        // An explicit "watch this one" beats the strategy outright.
        if let Some(login) = self.forced_channel.take() {
            match candidates
                .iter()
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
                        w.confidence = c.confidence;
                    }
                    return Ok(self.current.clone());
                }
                self.begin_watching(now, *c, reason);
                return Ok(self.current.clone());
            }
            Decision::Idle(reason) => {
                // Staying put is a decision, not a failure.
                if self.current.is_some() {
                    // Re-explain the incumbent under the strategy actually in
                    // force. The stored reason is whatever was true when the
                    // channel was picked, so after a strategy change the page
                    // read "availability: advances Madden Live" while the
                    // selector said "deadline" — the miner looking like it was
                    // ignoring the setting it had just been given.
                    let restated = current_id.as_ref().and_then(|id| {
                        candidates.iter().find(|c| &c.channel.id == id).map(|c| {
                            let world = WorldView {
                                now,
                                campaigns: &self.campaigns,
                                candidates: &candidates,
                                favourites: &self.favourites,
                                priority_games: &self.priority_games,
                                pinned: self.pinned.as_deref(),
                                current: current_id.as_ref(),
                            };
                            strategy::explain(c, &world, self.strategy.as_ref())
                        })
                    });
                    if let Some(restated) = restated
                        && let Some(w) = &mut self.current
                    {
                        w.reason = restated;
                    }
                    tracing::debug!(reason, "keeping the current channel");
                    return Ok(self.current.clone());
                }
                tracing::info!(reason, "no channel selected");
            }
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
    /// One list, ranked later by the strategy. Each candidate carries a
    /// [`Confidence`] recording whether Twitch named its drops outright or the
    /// miner inferred them from the directory. This used to return two lists
    /// and prefer the first, which meant that once Twitch stopped confirming
    /// anything the second was reached by a path that ignored the strategy.
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
    ) -> Result<Vec<Candidate>, ApiError> {
        let wants = self.wants(now);
        if wants.is_empty() {
            return Ok(Vec::new());
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
            let mut found = self.api.live_channels(&game, limit).await?;
            // Twitch does occasionally list the same channel twice in one
            // directory page. Left alone, the channel is mapped to the same
            // campaign once per appearance, so it looks like it advances that
            // campaign twice — inflating its coverage score, duplicating the
            // session's campaign list, and spending the per-campaign channel
            // budget on a channel already counted.
            let mut seen: HashSet<ChannelId> = HashSet::new();
            found.retain(|(c, _)| seen.insert(c.id.clone()));
            channels_by_game.insert(game.id.clone(), found);
        }

        // --- resolve short allow-lists directly -----------------------------
        //
        // A campaign locked to one small streamer is invisible to the
        // directory: they will not be on any page of it while they have a
        // handful of viewers. Asking about them by name is the only way to
        // find them, and these are exactly the exclusive campaigns users care
        // most about.
        // Deduplicated by channel and globally capped. Two campaigns sharing
        // a game and an allow-listed channel would otherwise resolve it twice
        // and push it into that game's list twice — which then had the channel
        // serving the same campaign twice over, inflating its apparent
        // coverage. And a per-campaign cap is not a cap at all: twenty
        // campaigns with eight allow-listed channels each is a hundred and
        // sixty lookups.
        let mut direct: BTreeMap<ChannelId, Channel> = BTreeMap::new();
        for want in wants.iter() {
            if want.allowed.is_empty()
                || want.allowed.len() > self.config.max_direct_allow_list_lookups
            {
                continue;
            }
            let known: HashSet<&ChannelId> = channels_by_game
                .get(&want.game.id)
                .map(|list| list.iter().map(|(c, _)| &c.id).collect())
                .unwrap_or_default();

            for channel in &want.allowed_channels {
                if known.contains(&channel.id)
                    || self.quarantine.is_quarantined(&channel.id.0, now)
                    || direct.len() >= self.config.max_direct_lookups_per_pass
                {
                    continue;
                }
                direct.insert(channel.id.clone(), channel.clone());
            }
        }

        if !direct.is_empty() {
            let resolved: Vec<(Channel, Option<Stream>)> =
                futures_util::stream::iter(direct.into_values().map(|channel| {
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
                        (channel, stream)
                    }
                }))
                .buffer_unordered(self.config.check_concurrency.max(1))
                .collect()
                .await;

            for (channel, stream) in resolved {
                let Some(stream) = stream else { continue };
                tracing::debug!(
                    channel = %channel.login,
                    "allow-listed channel found live outside the directory page"
                );
                // Only under the game it is actually streaming.
                //
                // This used to offer the channel under *every* game whose
                // allow-list named it, regardless of what was on screen. A
                // streamer allow-listed for a Rust campaign who is live
                // playing Just Chatting was therefore offered as a candidate
                // for the Rust campaign — and since these channels arrive from
                // `stream_info`, which reports no tags and so is taken as
                // drops-enabled, nothing downstream could catch it. Under
                // `Deadline` an urgent campaign would send the miner to watch
                // entirely the wrong stream for as long as the watchdog took
                // to notice it was earning nothing.
                //
                // The exception is the sitewide "any channel" categories,
                // which are earned regardless of what is on screen — the
                // reference implementation carries the same exemption.
                let streaming = stream.game.as_ref().map(|g| g.id.clone());
                let games: Vec<GameId> = wants
                    .iter()
                    .filter(|w| {
                        if !w.allowed.contains(&channel.id) {
                            return false;
                        }
                        ANY_CHANNEL_GAME_IDS.contains(&w.game.id.0.as_str())
                            || streaming.as_ref() == Some(&w.game.id)
                    })
                    .map(|w| w.game.id.clone())
                    .collect();
                if games.is_empty() {
                    tracing::debug!(
                        channel = %channel.login,
                        "allow-listed channel is live but streaming a different game"
                    );
                    continue;
                }

                for game in games {
                    let list = channels_by_game.entry(game).or_default();
                    if list.iter().any(|(c, _)| c.id == channel.id) {
                        continue;
                    }
                    list.push((channel.clone(), stream.clone()));
                }
            }
        }

        // --- map each channel to every campaign it could serve --------------
        let mut by_channel: BTreeMap<ChannelId, ChannelWork> = BTreeMap::new();
        let mut campaigns_without_channels = Vec::new();
        let mut live_counts: HashMap<String, usize> = HashMap::new();

        // Channels available to a campaign that is not tied to its own game.
        let everyone: Vec<(Channel, Stream)> = channels_by_game
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();

        for (i, want) in wants.iter().enumerate() {
            let any_channel = ANY_CHANNEL_GAME_IDS.contains(&want.game.id.0.as_str());
            let channels = if any_channel {
                &everyone
            } else {
                match channels_by_game.get(&want.game.id) {
                    Some(c) => c,
                    None => continue,
                }
            };
            // Recorded even when it stays zero: "we looked and found nobody" is
            // a different answer from "we never looked", and the dashboard can
            // only tell them apart if the campaign appears in this map.
            let seen = live_counts.entry(want.campaign.0.clone()).or_insert(0usize);
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
                // Counted for every eligible channel, not just the ones kept.
                // This is the answer to "who is live for this campaign?", which
                // is a question about Twitch, not about how much verifying the
                // miner chose to do.
                *seen += 1;
                // The directory returns the busiest streams first, and the
                // strategy only ever picks one channel. Verifying every one of
                // the thirty channels streaming a popular game costs requests
                // and buys nothing.
                //
                // A `continue` rather than a `break`: the rest of this loop is
                // pure bookkeeping over a list already in memory, and breaking
                // stopped the count dead at the cap — so the honest figure this
                // was meant to produce came out as nine for a game with thirty
                // channels live.
                if taken >= self.config.max_channels_per_campaign {
                    continue;
                }
                taken += 1;
                let entry = by_channel
                    .entry(channel.id.clone())
                    .or_insert_with(|| ChannelWork {
                        channel: channel.clone(),
                        stream: stream.clone(),
                        wants: Vec::new(),
                    });
                if !entry.wants.contains(&i) {
                    entry.wants.push(i);
                }
            }
            if !any {
                campaigns_without_channels.push(want.campaign.clone());
            }
        }
        self.live_counts = live_counts;

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

        // One request per twenty channels, rather than one per channel.
        //
        // These were sequential once, and 60 of them took 27 seconds against a
        // real account — during which the miner watched nothing at all. They
        // are independent read-only questions, so the transport answers them
        // in batches and the rate limiter keeps the burst inside what Twitch
        // tolerates.
        let ids: Vec<ChannelId> = work.iter().map(|w| w.channel.id.clone()).collect();
        let answers: HashMap<ChannelId, Result<Vec<DropId>, ApiError>> = self
            .api
            .available_drops_many(&ids)
            .await
            .into_iter()
            .collect();

        let results: Vec<(ChannelWork, Result<Vec<DropId>, ApiError>)> = work
            .into_iter()
            .map(|item| {
                let outcome = answers.get(&item.channel.id).cloned().unwrap_or_else(|| {
                    Err(ApiError::Malformed(
                        "the availability check returned no answer for this channel".into(),
                    ))
                });
                (item, outcome)
            })
            .collect();

        let mut candidates = Vec::new();
        let mut unusable = 0usize;

        for (item, outcome) in results {
            // What the channel serves on the strength of the directory alone:
            // it is streaming the campaign's game and passes its allow-list.
            let by_game: Vec<Advance> = item
                .wants
                .iter()
                .map(|i| Advance {
                    campaign: wants[*i].campaign.clone(),
                    drops: wants[*i].drops.iter().cloned().collect(),
                })
                .collect();

            // The per-channel check can only *add* to that.
            //
            // It used to replace it, which is why nothing was ever selected on
            // merit: Twitch stopped listing game campaigns here, so the
            // intersection was empty for every channel on every pass and the
            // strategy was never consulted at all. See [`Confidence`].
            let confirmed: Vec<Advance> = match &outcome {
                Ok(available) => {
                    let available: HashSet<&DropId> = available.iter().collect();
                    item.wants
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
                        .collect()
                }
                Err(e) => {
                    // Inconclusive, not negative. The directory still stands.
                    tracing::debug!(
                        channel = %item.channel.login,
                        error = %e,
                        "availability check failed; falling back to the directory tag"
                    );
                    Vec::new()
                }
            };

            // The confirmation is a *subset* of what the directory implies —
            // it can only name drops we already asked about. Taking it in
            // place of `by_game` therefore deleted campaigns from the
            // candidate: a channel Twitch confirmed for one of the four
            // campaigns it serves was recorded as serving one, scored as
            // serving one, and — worse — had the other three struck from
            // `CurrentWatch::campaigns`, so their progress events were
            // discarded as "not what we are mining" and the stall clock never
            // reset. Confirmation must add confidence, never remove work.
            let confidence = if !confirmed.is_empty() {
                Confidence::Confirmed
            } else if item.stream.drops_enabled {
                Confidence::Tagged
            } else {
                // No tag and no confirmation is the one case with nothing to
                // go on at all.
                unusable += 1;
                continue;
            };

            candidates.push(make_candidate(&item, by_game, &wants, confidence));
        }

        if unusable > 0 {
            tracing::debug!(
                unusable,
                "channels had neither a drops-enabled tag nor a confirmed drop"
            );
        }

        // Concurrency makes completion order arbitrary; sort so selection is
        // reproducible rather than a race. The strategy scores them properly;
        // this only settles ties.
        candidates.sort_by_key(|c: &Candidate| c.channel.login.clone());

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

        Ok(candidates)
    }

    fn begin_watching(&mut self, now: DateTime<Utc>, candidate: Candidate, reason: String) {
        // Moving straight from one channel to another skips `stop_watching`,
        // and the transport would otherwise go on playing the old one until
        // the next heartbeat named the new one.
        if self.current.is_some() {
            self.api.stop_watch();
        }
        self.progress.reset();
        self.progress.begin(now);
        // Resetting here as well as in `stop_watching` is what actually kept
        // the ladder pinned: recovery would switch channel, selection would
        // start a new episode, and the rungs above "switch channel" — resync,
        // revalidate the token, rest the campaign — were never reached.
        if std::mem::take(&mut self.carry_recovery) {
            tracing::debug!(
                rung = self.watchdog.rungs_attempted(),
                "continuing the recovery episode on a new channel"
            );
        } else {
            self.watchdog.reset();
            self.recovery_step = None;
        }
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
            confidence: candidate.confidence,
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
    /// Records the claim id Twitch pushed for an earned drop.
    ///
    /// Both lists, because a drop can be earned in a campaign the miner will
    /// never *mine*: one that expired within the claim window, or a
    /// subscription reward the user subscribed for. Walking only the mineable
    /// campaigns dropped the claim id on the floor, and since a sub reward
    /// needs zero minutes its `is_claimable()` depends on that id alone — so
    /// the badge was never claimed at all unless a later sync happened to find
    /// `dropInstanceID` populated, which Twitch often omits.
    pub fn on_drop_claimable(&mut self, drop_id: &DropId, claim_id: &str) {
        for campaign in self.campaigns.iter_mut().chain(self.claim_only.iter_mut()) {
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
        let mut claimed_anything = false;
        let ready: Vec<(CampaignId, DropId, Option<String>, String)> = self
            .campaigns
            .iter()
            .chain(self.claim_only.iter())
            .flat_map(|c| c.drops.iter().map(move |d| (c, d)))
            .filter(|(_, d)| d.is_claimable())
            // A claim Twitch has already refused is rested rather than re-sent
            // every minute for the life of the campaign.
            .filter(|(_, d)| {
                self.claim_backoff
                    .get(&d.id)
                    .is_none_or(|(until, _)| now >= *until)
            })
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
                let strikes = self
                    .claim_backoff
                    .get(&drop_id)
                    .map_or(1, |(_, n)| n.saturating_add(1));
                // 1, 2, 4 ... minutes, capped. A refusal that is permanent
                // must not become a permanent trickle of requests.
                let wait = Duration::minutes((1i64 << strikes.min(8)).min(120));
                self.claim_backoff
                    .insert(drop_id.clone(), (now + wait, strikes));

                tracing::warn!(
                    drop = %drop_id,
                    error = %e,
                    retry_in_mins = wait.num_minutes(),
                    "claim refused; leaving it unclaimed so it is retried"
                );
                self.emit(
                    now,
                    EventKind::Notice {
                        message: format!(
                            "Could not claim {name}: {e}. Retrying in {} minutes.",
                            wait.num_minutes()
                        ),
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
                .chain(self.claim_only.iter_mut())
            {
                if let Some(d) = campaign.drops.iter_mut().find(|d| d.id == drop_id) {
                    d.is_claimed = true;
                    d.claim_id = None;
                }
            }
            self.claim_backoff.remove(&drop_id);
            claimed_anything = true;
            self.emit(now, EventKind::DropClaimed { drop: drop_id, name });
        }

        // A claim can be the last thing a channel had to give, and nothing else
        // notices. Observed live on 2026-08-24: the final tier of both campaigns
        // a channel was chosen for confirmed at 19:57, both were claimed at
        // 19:58, and the miner then heartbeated that stream for seven more
        // minutes. The sync that would have dropped the campaigns was ten
        // minutes away, and the watchdog reads a live, healthy stream as
        // healthy — correctly, because this is not a stall. It is a finished
        // job, and the only honest response to it is to go and find another.
        if claimed_anything && self.current.is_some() && !self.current_can_still_earn() {
            tracing::info!("everything this channel was chosen for is claimed; reselecting");
            self.stop_watching(now, StopReason::Finished);
        }
        Ok(())
    }
}

/// How long after a campaign ends its earned drops can still be claimed.
/// Twitch's own documented grace period.
const CLAIM_GRACE: Duration = Duration::hours(24);

/// How far ahead a campaign is worth waking up for. Beyond this the next
/// scheduled sync will notice it long before it opens.
const UPCOMING_HORIZON: Duration = Duration::hours(24);

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
fn make_candidate(
    item: &ChannelWork,
    mut advances: Vec<Advance>,
    wants: &[Want],
    confidence: Confidence,
) -> Candidate {
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
        confidence,
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
        starts_at: campaign.starts_at,
        ends_at: campaign.ends_at,
        box_art_url: campaign.game.box_art_url.clone(),
    }
}
