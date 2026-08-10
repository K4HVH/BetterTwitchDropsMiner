//! Channel selection strategies.
//!
//! A strategy is a pure function over a world snapshot, which is what makes it
//! unit-testable against synthetic scenarios ("47 campaigns, 3 expiring
//! tonight, 2 channels live") and comparable against other strategies without
//! touching Twitch.
//!
//! Every strategy shares the same pipeline: feasibility filter, strategy
//! score, tie-break, hysteresis. Only the score varies.
//!
//! # Candidates are channels, not channel-campaign pairs
//!
//! One Twitch channel routinely advances several campaigns at the same time —
//! multiple campaigns run for the same game, and one hour of watching counts
//! for all of them. An earlier version of this module modelled a candidate as
//! `(channel, one campaign)`, which meant a channel serving four campaigns
//! appeared as four separate candidates, each scored as though it earned a
//! quarter of what it really did. `Coverage` in particular could not see
//! coverage at all: it counted drops *within* a single campaign.
//!
//! So a [`Candidate`] is a channel plus every [`Advance`] it makes, and every
//! score is a function of that whole set.

use crate::eligibility;
use crate::model::*;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;

/// One campaign a channel would advance, and which of its drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    pub campaign: CampaignId,
    /// Drops this channel would actually progress, per the server.
    pub drops: Vec<DropId>,
}

/// A channel that could be watched, and everything it would earn at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub channel: Channel,
    pub stream: Stream,
    /// Every campaign this one channel advances, most urgent first.
    pub advances: Vec<Advance>,
}

impl Candidate {
    /// The campaign used for display and for progress scoping. Advances are
    /// ordered most-urgent-first, so this is the one whose deadline drove the
    /// choice.
    pub fn primary_campaign(&self) -> Option<&CampaignId> {
        self.advances.first().map(|a| &a.campaign)
    }

    pub fn campaigns(&self) -> impl Iterator<Item = &CampaignId> {
        self.advances.iter().map(|a| &a.campaign)
    }

    pub fn campaign_count(&self) -> usize {
        self.advances.len()
    }

    /// Total drops advanced across every campaign.
    pub fn drop_count(&self) -> usize {
        self.advances.iter().map(|a| a.drops.len()).sum()
    }

    pub fn drops(&self) -> impl Iterator<Item = &DropId> {
        self.advances.iter().flat_map(|a| &a.drops)
    }

    pub fn advances_campaign(&self, id: &CampaignId) -> bool {
        self.advances.iter().any(|a| &a.campaign == id)
    }

    /// Keeps only the advances the predicate accepts. Returns `None` when
    /// nothing is left, meaning the channel is no longer worth watching.
    pub fn retaining(&self, keep: impl Fn(&Advance) -> bool) -> Option<Candidate> {
        let advances: Vec<Advance> = self.advances.iter().filter(|a| keep(a)).cloned().collect();
        (!advances.is_empty()).then(|| Candidate {
            channel: self.channel.clone(),
            stream: self.stream.clone(),
            advances,
        })
    }
}

/// Everything a strategy is allowed to know.
pub struct WorldView<'a> {
    pub now: DateTime<Utc>,
    pub campaigns: &'a [DropCampaign],
    pub candidates: &'a [Candidate],
    /// Channel logins to prefer, lowercased.
    pub favourites: &'a HashSet<String>,
    /// Game names in priority order, lowercased.
    pub priority_games: &'a [String],
    /// A specific channel login the user pinned.
    pub pinned: Option<&'a str>,
    /// What is being watched right now, for hysteresis.
    pub current: Option<&'a ChannelId>,
}

impl WorldView<'_> {
    pub fn campaign(&self, id: &CampaignId) -> Option<&DropCampaign> {
        self.campaigns.iter().find(|c| &c.id == id)
    }

    /// Campaigns a candidate advances that we can still resolve. A candidate
    /// referring only to campaigns we no longer hold scores nothing.
    fn advanced_campaigns<'b>(&'b self, c: &'b Candidate) -> impl Iterator<Item = &'b DropCampaign> {
        c.campaigns().filter_map(|id| self.campaign(id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Watch(Box<Candidate>),
    /// Nothing worth watching, with a human-readable reason.
    Idle(String),
}

/// How much spare time a campaign has: time until its deadline minus the
/// watching still needed. Negative means it is already impossible.
pub fn slack(campaign: &DropCampaign, now: DateTime<Utc>) -> Duration {
    let needed = Duration::minutes(eligibility::remaining_minutes(campaign) as i64);
    (campaign.ends_at - now) - needed
}

/// Whether a campaign can still be finished before it expires.
///
/// The reference implementation has no such notion and will happily consume
/// its single watch slot grinding at a campaign that cannot possibly complete.
pub fn is_feasible(campaign: &DropCampaign, now: DateTime<Utc>) -> bool {
    slack(campaign, now) >= Duration::zero()
}

pub trait MiningStrategy: Send + Sync {
    fn name(&self) -> &'static str;

    /// Higher is better. `None` means never pick this candidate.
    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64>;

    /// Whether infeasible campaigns should be skipped. Only the manual
    /// strategy overrides this — if the user pins a channel, that is an
    /// instruction, not a suggestion.
    fn respects_feasibility(&self) -> bool {
        true
    }
}

/// Bonus applied when a channel is on the favourites list. Large enough to
/// break ties, too small to override a real deadline difference — favourites
/// are a preference, not an override.
const FAVOURITE_BONUS: f64 = 5.0;
/// Bonus per *extra* campaign advanced at the same time. One hour of watching
/// that counts for four campaigns is worth roughly four hours of watching that
/// counts for one, and this is where that shows up under every strategy.
const EXTRA_CAMPAIGN_BONUS: f64 = 3.0;
/// A challenger must beat the incumbent by this much to trigger a switch,
/// so the miner does not thrash between two near-equal channels.
const HYSTERESIS_MARGIN: f64 = 10.0;

/// Runs the shared pipeline and returns what to do.
///
/// The returned candidate is *pruned*: infeasible campaigns are stripped from
/// its advances, so the caller records only what this channel can really earn.
/// A channel serving one dead campaign and one live one stays selectable —
/// dropping the whole candidate would throw away the live one too.
pub fn choose(strategy: &dyn MiningStrategy, world: &WorldView) -> Decision {
    if world.candidates.is_empty() {
        return Decision::Idle("no live channel can progress any eligible drop".into());
    }

    let mut scored: Vec<(f64, Candidate)> = Vec::new();
    let mut skipped_infeasible = 0usize;

    for candidate in world.candidates {
        let pruned = if strategy.respects_feasibility() {
            match candidate.retaining(|a| {
                world
                    .campaign(&a.campaign)
                    .is_none_or(|c| is_feasible(c, world.now))
            }) {
                Some(p) => p,
                None => {
                    skipped_infeasible += 1;
                    continue;
                }
            }
        } else {
            candidate.clone()
        };

        let Some(base) = strategy.score(&pruned, world) else {
            continue;
        };
        scored.push((base + tie_break_bonus(&pruned, world), pruned));
    }

    if scored.is_empty() {
        return Decision::Idle(if skipped_infeasible > 0 {
            format!("{skipped_infeasible} candidate(s) cannot finish before their deadline")
        } else {
            "no candidate scored".into()
        });
    }

    // Deterministic ordering: score, then coverage, then login. Never hash order.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.drop_count().cmp(&a.1.drop_count()))
            .then(a.1.channel.login.cmp(&b.1.channel.login))
    });

    let best_score = scored[0].0;

    // Hysteresis: stay put unless the challenger is clearly better.
    if let Some(current) = world.current
        && &scored[0].1.channel.id != current
        && let Some((incumbent_score, _)) = scored.iter().find(|(_, c)| &c.channel.id == current)
        && best_score - incumbent_score < HYSTERESIS_MARGIN
    {
        return Decision::Idle("staying on the current channel".into());
    }

    Decision::Watch(Box::new(scored.swap_remove(0).1))
}

/// Applied under every strategy: favourites first, then breadth of coverage,
/// then a whisker of stream stability.
fn tie_break_bonus(candidate: &Candidate, world: &WorldView) -> f64 {
    let mut bonus = 0.0;
    if world
        .favourites
        .contains(&candidate.channel.login.to_lowercase())
    {
        bonus += FAVOURITE_BONUS;
    }
    // Each extra drop advanced simultaneously is free parallelism...
    bonus += candidate.drop_count() as f64;
    // ...and an extra whole campaign is worth more than an extra tier.
    bonus += candidate.campaign_count().saturating_sub(1) as f64 * EXTRA_CAMPAIGN_BONUS;
    // Viewer count as a stability proxy, deliberately tiny.
    bonus += (candidate.stream.viewers as f64).ln_1p() * 0.01;
    bonus
}

/// Why this channel was chosen, in words, for the dashboard and the log.
///
/// "Silently doing something inexplicable" is one of the complaints this
/// project exists to answer, so every selection can account for itself.
pub fn explain(candidate: &Candidate, world: &WorldView, strategy: &dyn MiningStrategy) -> String {
    let mut parts = Vec::new();

    let campaigns = candidate.campaign_count();
    if campaigns > 1 {
        parts.push(format!(
            "advances {campaigns} campaigns at once ({} drops)",
            candidate.drop_count()
        ));
    } else if let Some(c) = candidate.primary_campaign().and_then(|id| world.campaign(id)) {
        parts.push(format!("advances {}", c.name));
    }

    if let Some(soonest) = world
        .advanced_campaigns(candidate)
        .min_by_key(|c| slack(c, world.now))
    {
        let hours = slack(soonest, world.now).num_hours();
        parts.push(if hours < 48 {
            format!("soonest deadline has {hours}h of slack")
        } else {
            format!("soonest deadline has {}d of slack", hours / 24)
        });
    }

    if world
        .favourites
        .contains(&candidate.channel.login.to_lowercase())
    {
        parts.push("a favourite channel".into());
    }
    if world.pinned.is_some_and(|p| {
        p.eq_ignore_ascii_case(&candidate.channel.login)
    }) {
        parts.push("pinned by you".into());
    }

    format!("{}: {}", strategy.name(), parts.join("; "))
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Earliest deadline first, over campaigns that can still be finished.
/// Nothing achievable is lost to expiry.
///
/// Scored on the *most urgent* campaign the channel advances, so a channel
/// that happens to also rescue something expiring tonight is preferred over
/// one that only serves a comfortable campaign.
pub struct Deadline;

impl MiningStrategy for Deadline {
    fn name(&self) -> &'static str {
        "deadline"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let tightest = world
            .advanced_campaigns(candidate)
            .map(|c| slack(c, world.now))
            .min()?;
        // Less slack scores higher; scaled to hours so the numbers stay sane.
        Some(1000.0 - (tightest.num_minutes() as f64 / 60.0))
    }
}

/// The user's chosen games first, then everything else by urgency.
///
/// A channel is ranked by the best-placed game it advances, so one stream
/// serving both the top pick and a lower one is never penalised for the latter.
///
/// Games that are not on the list are still mined, ranked below every listed
/// one. The strict variant used to be the only behaviour, and it meant a user
/// whose listed games happened to have nobody streaming them sat idle earning
/// nothing while dozens of other campaigns ticked away — see [`PriorityOnly`]
/// for when that is genuinely what you want.
pub struct Priority;

/// The rank of a game not on the priority list. Everything listed scores
/// above this, and unlisted games are separated among themselves by urgency.
const UNLISTED_BASE: f64 = 0.0;

fn priority_rank(candidate: &Candidate, world: &WorldView) -> Option<usize> {
    world
        .advanced_campaigns(candidate)
        .filter_map(|c| {
            let game = c.game.name.to_lowercase();
            world.priority_games.iter().position(|g| *g == game)
        })
        .min()
}

/// Urgency as a small tie-break for unlisted games: soonest deadline first,
/// scaled to stay well below any listed game's score.
fn urgency_fraction(candidate: &Candidate, world: &WorldView) -> f64 {
    world
        .advanced_campaigns(candidate)
        .map(|c| slack(c, world.now))
        .min()
        .map(|d| 1.0 / (1.0 + (d.num_minutes().max(0) as f64 / 1440.0)))
        .unwrap_or(0.0)
}

impl MiningStrategy for Priority {
    fn name(&self) -> &'static str {
        "priority"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        match priority_rank(candidate, world) {
            // Earlier in the list scores higher; every listed game outranks
            // every unlisted one.
            Some(rank) => Some(1000.0 + (world.priority_games.len() - rank) as f64 * 100.0),
            None => Some(UNLISTED_BASE + urgency_fraction(candidate, world)),
        }
    }
}

/// Only the user's chosen games, and nothing else — even if that means idling.
pub struct PriorityOnly;

impl MiningStrategy for PriorityOnly {
    fn name(&self) -> &'static str {
        "priority-only"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let rank = priority_rank(candidate, world)?;
        Some((world.priority_games.len() - rank) as f64 * 100.0)
    }
}

/// Least headroom first: time left divided by watching still needed.
///
/// Where `deadline` compares absolute spare time, this compares it *relative*
/// to the work outstanding — so a campaign with six hours left and five hours
/// of watching to do outranks one with two days left and ten minutes to do.
/// Across a mix of short and long campaigns that is the better question.
pub struct Availability;

impl MiningStrategy for Availability {
    fn name(&self) -> &'static str {
        "availability"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let tightest = world
            .advanced_campaigns(candidate)
            .map(|c| {
                let needed = eligibility::remaining_minutes(c) as f64;
                let left = (c.ends_at - world.now).num_minutes() as f64;
                if needed <= 0.0 {
                    f64::INFINITY
                } else {
                    left / needed
                }
            })
            .fold(f64::INFINITY, f64::min);

        if !tightest.is_finite() {
            // Nothing left to earn here; never the best use of the slot.
            return Some(0.0);
        }
        // Less headroom scores higher.
        Some(1000.0 - tightest)
    }
}

/// Prefer the channel advancing the most campaigns at once.
///
/// Campaigns dominate drops: finishing four campaigns' worth of tiers in
/// parallel beats grinding four tiers of one campaign.
pub struct Coverage;

impl MiningStrategy for Coverage {
    fn name(&self) -> &'static str {
        "coverage"
    }

    fn score(&self, candidate: &Candidate, _world: &WorldView) -> Option<f64> {
        Some(candidate.campaign_count() as f64 * 100.0 + candidate.drop_count() as f64 * 10.0)
    }
}

/// Finish one campaign entirely before moving on, minimising half-done work.
/// Prefers whatever is closest to completion.
pub struct Completionist;

impl MiningStrategy for Completionist {
    fn name(&self) -> &'static str {
        "completionist"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let nearest = world
            .advanced_campaigns(candidate)
            .map(eligibility::remaining_minutes)
            .min()?;
        // Fewer minutes left scores higher.
        Some(1000.0 - nearest as f64)
    }
}

/// Watch exactly what the user pinned, ignoring everything else.
pub struct Manual;

impl MiningStrategy for Manual {
    fn name(&self) -> &'static str {
        "manual"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let pinned = world.pinned?;
        candidate
            .channel
            .login
            .eq_ignore_ascii_case(pinned)
            .then_some(1000.0)
    }

    /// A pin is an instruction. If the user wants to watch a channel whose
    /// campaign cannot finish, that is their call to make.
    fn respects_feasibility(&self) -> bool {
        false
    }
}

pub fn by_name(name: &str) -> Option<Box<dyn MiningStrategy>> {
    match name.to_lowercase().as_str() {
        "deadline" => Some(Box::new(Deadline)),
        "availability" => Some(Box::new(Availability)),
        "priority" => Some(Box::new(Priority)),
        "priority-only" | "priority_only" => Some(Box::new(PriorityOnly)),
        "coverage" => Some(Box::new(Coverage)),
        "completionist" => Some(Box::new(Completionist)),
        "manual" => Some(Box::new(Manual)),
        _ => None,
    }
}

pub const ALL_STRATEGIES: [&str; 7] = [
    "deadline",
    "availability",
    "priority",
    "priority-only",
    "coverage",
    "completionist",
    "manual",
];

/// One line describing what each strategy does, for the dashboard's picker.
pub fn describe(name: &str) -> &'static str {
    match name {
        "deadline" => "Never lose an achievable campaign — always mine whatever expires soonest.",
        "availability" => {
            "Mine whatever has the least breathing room: time left measured against work left."
        }
        "priority" => "Your game list first, top down, then everything else by urgency.",
        "priority-only" => "Only your listed games. Sits idle rather than mining anything else.",
        "coverage" => "Mine the channel that advances the most campaigns simultaneously.",
        "completionist" => "Finish the campaign closest to done before starting another.",
        "manual" => "Watch only the channel you pinned.",
        _ => "Unknown strategy.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
    }

    fn game(name: &str) -> Game {
        Game {
            id: GameId::from(name),
            name: name.into(),
            slug: name.to_lowercase(),
            box_art_url: None,
        }
    }

    fn campaign(id: &str, game_name: &str, ends_in: Duration, needed: u32) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from(id),
            name: id.into(),
            game: game(game_name),
            starts_at: now() - Duration::days(1),
            ends_at: now() + ends_in,
            is_account_linked: true,
            allowed_channels: vec![],
            account_link_url: None,
            drops: vec![TimedDrop {
                id: DropId::from(&format!("{id}-d1")[..]),
                name: "d".into(),
                benefits: vec![],
                required_minutes: needed,
                current_minutes: 0,
                claim_id: None,
                is_claimed: false,
                precondition_ids: vec![],
            }],
        }
    }

    /// A candidate advancing a single campaign, the common case.
    fn candidate(login: &str, campaign_id: &str, drops: usize) -> Candidate {
        multi(login, &[(campaign_id, drops)])
    }

    /// A candidate advancing several campaigns at once.
    fn multi(login: &str, spec: &[(&str, usize)]) -> Candidate {
        Candidate {
            channel: Channel {
                id: ChannelId::from(login),
                login: login.into(),
                display_name: login.into(),
            },
            stream: Stream {
                broadcast_id: "b".into(),
                game: Some(game("G")),
                title: "t".into(),
                viewers: 100,
                drops_enabled: true,
            },
            advances: spec
                .iter()
                .map(|(cid, n)| Advance {
                    campaign: CampaignId::from(*cid),
                    drops: (0..*n)
                        .map(|i| DropId::from(&format!("{cid}-d{i}")[..]))
                        .collect(),
                })
                .collect(),
        }
    }

    fn world<'a>(
        campaigns: &'a [DropCampaign],
        candidates: &'a [Candidate],
        favourites: &'a HashSet<String>,
        priority: &'a [String],
    ) -> WorldView<'a> {
        WorldView {
            now: now(),
            campaigns,
            candidates,
            favourites,
            priority_games: priority,
            pinned: None,
            current: None,
        }
    }

    fn watched(d: &Decision) -> &str {
        match d {
            Decision::Watch(c) => &c.channel.login,
            Decision::Idle(reason) => panic!("expected a choice, got idle: {reason}"),
        }
    }

    fn chosen(d: &Decision) -> &Candidate {
        match d {
            Decision::Watch(c) => c,
            Decision::Idle(reason) => panic!("expected a choice, got idle: {reason}"),
        }
    }

    #[test]
    fn feasibility_rejects_what_cannot_finish_in_time() {
        // Needs 240 minutes, 3 hours left.
        let c = campaign("c1", "G", Duration::hours(3), 240);
        assert!(!is_feasible(&c, now()));
        assert!(slack(&c, now()) < Duration::zero());

        // Needs 60 minutes, 3 hours left.
        let ok = campaign("c2", "G", Duration::hours(3), 60);
        assert!(is_feasible(&ok, now()));
    }

    #[test]
    fn an_impossible_campaign_is_never_chosen_and_the_reason_is_given() {
        let campaigns = vec![campaign("c1", "G", Duration::hours(1), 600)];
        let candidates = vec![candidate("s1", "c1", 1)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        match choose(&Deadline, &w) {
            Decision::Idle(reason) => assert!(
                reason.contains("deadline"),
                "the reason must explain itself, got: {reason}"
            ),
            other => panic!("expected idle, got {other:?}"),
        }
    }

    #[test]
    fn deadline_prefers_the_campaign_closest_to_being_lost() {
        let campaigns = vec![
            campaign("c-late", "G", Duration::days(20), 60),
            campaign("c-soon", "G", Duration::hours(4), 60),
        ];
        let candidates = vec![candidate("late", "c-late", 1), candidate("soon", "c-soon", 1)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Deadline, &w)), "soon");
    }

    #[test]
    fn priority_follows_the_users_list_order() {
        let campaigns = vec![
            campaign("c-a", "Alpha", Duration::days(5), 60),
            campaign("c-b", "Beta", Duration::hours(4), 60),
        ];
        let candidates = vec![candidate("a", "c-a", 1), candidate("b", "c-b", 1)];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string(), "beta".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert_eq!(
            watched(&choose(&Priority, &w)),
            "a",
            "Alpha outranks Beta even though Beta expires sooner"
        );
    }

    #[test]
    fn priority_only_ignores_games_not_on_the_list() {
        let campaigns = vec![campaign("c-x", "Unlisted", Duration::days(5), 60)];
        let candidates = vec![candidate("x", "c-x", 1)];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert!(matches!(choose(&PriorityOnly, &w), Decision::Idle(_)));
    }

    /// The default must never sit idle earning nothing just because the games
    /// on the list happen to have nobody streaming them.
    #[test]
    fn priority_still_mines_unlisted_games_rather_than_idling() {
        let campaigns = vec![campaign("c-x", "Unlisted", Duration::days(5), 60)];
        let candidates = vec![candidate("x", "c-x", 1)];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert_eq!(watched(&choose(&Priority, &w)), "x");
    }

    /// ...but a listed game always beats an unlisted one, however urgent.
    #[test]
    fn a_listed_game_outranks_any_unlisted_one() {
        let campaigns = vec![
            campaign("c-listed", "Alpha", Duration::days(30), 60),
            campaign("c-unlisted", "Whatever", Duration::hours(3), 60),
        ];
        let candidates = vec![
            candidate("listed", "c-listed", 1),
            candidate("unlisted", "c-unlisted", 1),
        ];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert_eq!(watched(&choose(&Priority, &w)), "listed");
    }

    /// Among unlisted games, the one closest to being lost wins.
    #[test]
    fn unlisted_games_are_ordered_by_urgency() {
        let campaigns = vec![
            campaign("c-later", "Beta", Duration::days(20), 60),
            campaign("c-sooner", "Gamma", Duration::hours(5), 60),
        ];
        let candidates = vec![
            candidate("later", "c-later", 1),
            candidate("sooner", "c-sooner", 1),
        ];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert_eq!(watched(&choose(&Priority, &w)), "sooner");
    }

    /// Availability compares headroom *relative to the work left*, which is
    /// the difference from `deadline`.
    #[test]
    fn availability_prefers_the_campaign_with_the_least_headroom() {
        let campaigns = vec![
            // Two days left, ten minutes of watching: loads of headroom.
            campaign("c-easy", "G", Duration::days(2), 10),
            // Six hours left, five hours of watching: tight.
            campaign("c-tight", "G", Duration::hours(6), 300),
        ];
        let candidates = vec![
            candidate("easy", "c-easy", 1),
            candidate("tight", "c-tight", 1),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Availability, &w)), "tight");
    }

    /// Where the two strategies actually disagree, and why `availability`
    /// exists at all.
    ///
    /// - `quick` ends in 100 minutes and needs 10: 90 minutes of slack, but
    ///   ten times more time than it needs.
    /// - `grind` ends in 1000 minutes and needs 900: more slack in absolute
    ///   terms, but almost none relative to the work.
    ///
    /// `deadline` protects the nearer wall-clock deadline; `availability`
    /// protects the one that will actually be tight to finish.
    #[test]
    fn availability_and_deadline_disagree_on_headroom_versus_slack() {
        let campaigns = vec![
            campaign("c-quick", "G", Duration::minutes(100), 10),
            campaign("c-grind", "G", Duration::minutes(1000), 900),
        ];
        let candidates = vec![
            candidate("quick", "c-quick", 1),
            candidate("grind", "c-grind", 1),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Deadline, &w)), "quick", "90 min of slack beats 100");
        assert_eq!(
            watched(&choose(&Availability, &w)),
            "grind",
            "a ratio of 1.1 is far tighter than one of 10"
        );
    }

    #[test]
    fn completionist_finishes_what_is_closest_to_done() {
        let campaigns = vec![
            campaign("c-far", "G", Duration::days(5), 600),
            campaign("c-near", "G", Duration::days(5), 30),
        ];
        let candidates = vec![candidate("far", "c-far", 1), candidate("near", "c-near", 1)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Completionist, &w)), "near");
    }

    #[test]
    fn manual_watches_only_the_pinned_channel() {
        let campaigns = vec![campaign("c1", "G", Duration::days(5), 60)];
        let candidates = vec![candidate("other", "c1", 3), candidate("pinned", "c1", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        w.pinned = Some("pinned");

        assert_eq!(watched(&choose(&Manual, &w)), "pinned");
    }

    /// A pin is an instruction, not a suggestion.
    #[test]
    fn manual_overrides_the_feasibility_filter() {
        let campaigns = vec![campaign("c1", "G", Duration::hours(1), 600)];
        let candidates = vec![candidate("pinned", "c1", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        w.pinned = Some("pinned");

        assert_eq!(watched(&choose(&Manual, &w)), "pinned");
    }

    #[test]
    fn manual_matches_the_pin_case_insensitively() {
        let campaigns = vec![campaign("c1", "G", Duration::days(5), 60)];
        let candidates = vec![candidate("SomeStreamer", "c1", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        w.pinned = Some("somestreamer");

        assert_eq!(watched(&choose(&Manual, &w)), "SomeStreamer");
    }

    #[test]
    fn manual_idles_when_the_pinned_channel_is_offline() {
        let campaigns = vec![campaign("c1", "G", Duration::days(5), 60)];
        let candidates = vec![candidate("someone-else", "c1", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        w.pinned = Some("not-live");

        assert!(matches!(choose(&Manual, &w), Decision::Idle(_)));
    }

    /// Favourites apply under every strategy, but only as a tie-break.
    #[test]
    fn a_favourite_wins_a_tie() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(5), 60),
            campaign("c-b", "G", Duration::days(5), 60),
        ];
        let candidates = vec![candidate("plain", "c-a", 1), candidate("beloved", "c-b", 1)];
        let favs: HashSet<String> = ["beloved".to_string()].into_iter().collect();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Deadline, &w)), "beloved");
    }

    #[test]
    fn a_favourite_does_not_override_a_real_deadline_difference() {
        let campaigns = vec![
            campaign("c-urgent", "G", Duration::hours(2), 60),
            campaign("c-relaxed", "G", Duration::days(20), 60),
        ];
        let candidates = vec![
            candidate("urgent", "c-urgent", 1),
            candidate("beloved", "c-relaxed", 1),
        ];
        let favs: HashSet<String> = ["beloved".to_string()].into_iter().collect();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(
            watched(&choose(&Deadline, &w)),
            "urgent",
            "a favourite is a preference, not an override"
        );
    }

    #[test]
    fn favourites_match_case_insensitively() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(5), 60),
            campaign("c-b", "G", Duration::days(5), 60),
        ];
        let candidates = vec![candidate("plain", "c-a", 1), candidate("beloved", "c-b", 1)];
        let favs: HashSet<String> = ["BeLoVeD".to_lowercase()].into_iter().collect();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Deadline, &w)), "beloved");
    }

    /// Without hysteresis the miner flaps between two near-equal channels,
    /// losing drop credit on every switch.
    #[test]
    fn hysteresis_keeps_the_miner_on_a_near_equal_incumbent() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(5), 60),
            campaign("c-b", "G", Duration::days(5) - Duration::minutes(1), 60),
        ];
        let candidates = vec![candidate("incumbent", "c-a", 1), candidate("rival", "c-b", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        let incumbent = ChannelId::from("incumbent");
        w.current = Some(&incumbent);

        assert!(
            matches!(choose(&Deadline, &w), Decision::Idle(_)),
            "a one-minute difference must not trigger a switch"
        );
    }

    #[test]
    fn a_clearly_better_channel_still_wins_despite_hysteresis() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(20), 60),
            campaign("c-b", "G", Duration::hours(3), 60),
        ];
        let candidates = vec![candidate("incumbent", "c-a", 1), candidate("urgent", "c-b", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        let incumbent = ChannelId::from("incumbent");
        w.current = Some(&incumbent);

        assert_eq!(watched(&choose(&Deadline, &w)), "urgent");
    }

    /// If the incumbent disappears from the candidate list, hysteresis must
    /// not pin the miner to a channel that is no longer available.
    #[test]
    fn hysteresis_does_not_apply_when_the_incumbent_is_gone() {
        let campaigns = vec![campaign("c-b", "G", Duration::days(5), 60)];
        let candidates = vec![candidate("replacement", "c-b", 1)];
        let favs = HashSet::new();
        let mut w = world(&campaigns, &candidates, &favs, &[]);
        let gone = ChannelId::from("vanished");
        w.current = Some(&gone);

        assert_eq!(watched(&choose(&Deadline, &w)), "replacement");
    }

    #[test]
    fn no_candidates_means_idle_with_a_reason() {
        let favs = HashSet::new();
        let w = world(&[], &[], &favs, &[]);
        match choose(&Deadline, &w) {
            Decision::Idle(reason) => assert!(!reason.is_empty()),
            other => panic!("expected idle, got {other:?}"),
        }
    }

    #[test]
    fn selection_is_deterministic_for_identical_candidates() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(5), 60),
            campaign("c-b", "G", Duration::days(5), 60),
        ];
        let candidates = vec![candidate("zeta", "c-a", 1), candidate("alpha", "c-b", 1)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        // Ties break on login, never on hash iteration order.
        assert_eq!(watched(&choose(&Deadline, &w)), "alpha");
        assert_eq!(watched(&choose(&Deadline, &w)), "alpha");
    }

    #[test]
    fn every_advertised_strategy_resolves_by_name() {
        for name in ALL_STRATEGIES {
            assert_eq!(by_name(name).expect("strategy exists").name(), name);
            assert!(!describe(name).starts_with("Unknown"));
        }
        assert!(by_name("nonsense").is_none());
    }

    // -----------------------------------------------------------------------
    // Cross-campaign behaviour
    //
    // These are the tests the old `(channel, one campaign)` model could not
    // express, and the reason it was replaced.
    // -----------------------------------------------------------------------

    /// The headline case: one hour on a channel serving four campaigns beats
    /// one hour on a channel serving one, even under a strategy that is not
    /// nominally about coverage.
    #[test]
    fn a_channel_advancing_several_campaigns_beats_an_equal_single_campaign_one() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::days(5), 60),
            campaign("c-b", "G", Duration::days(5), 60),
            campaign("c-c", "G", Duration::days(5), 60),
            campaign("c-d", "G", Duration::days(5), 60),
        ];
        let candidates = vec![
            candidate("narrow", "c-a", 1),
            multi("broad", &[("c-b", 1), ("c-c", 1), ("c-d", 1)]),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        let d = choose(&Deadline, &w);
        assert_eq!(watched(&d), "broad");
        assert_eq!(chosen(&d).campaign_count(), 3);
        assert_eq!(chosen(&d).drop_count(), 3);
    }

    /// Coverage must count *campaigns*, not drops within one campaign. Under
    /// the old model this test could not even be written: a candidate had a
    /// single campaign field, so `coverage` was really "most tiers".
    #[test]
    fn coverage_counts_campaigns_not_just_drops() {
        let campaigns = vec![
            campaign("c-fat", "G", Duration::days(5), 60),
            campaign("c-1", "G", Duration::days(5), 60),
            campaign("c-2", "G", Duration::days(5), 60),
        ];
        // Six tiers of one campaign, versus two campaigns of one tier each.
        let candidates = vec![
            candidate("one-fat-campaign", "c-fat", 6),
            multi("two-campaigns", &[("c-1", 1), ("c-2", 1)]),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Coverage, &w)), "two-campaigns");
    }

    /// A channel serving a dead campaign and a live one must stay selectable,
    /// with the dead campaign stripped from what we record as being mined.
    #[test]
    fn an_infeasible_campaign_is_pruned_rather_than_sinking_the_whole_channel() {
        let campaigns = vec![
            campaign("c-dead", "G", Duration::hours(1), 600),
            campaign("c-live", "G", Duration::days(5), 60),
        ];
        let candidates = vec![multi("mixed", &[("c-dead", 2), ("c-live", 1)])];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        let d = choose(&Deadline, &w);
        assert_eq!(watched(&d), "mixed");
        let picked = chosen(&d);
        assert_eq!(
            picked.campaign_count(),
            1,
            "the impossible campaign must not be recorded as being mined"
        );
        assert_eq!(picked.primary_campaign().unwrap().0, "c-live");
    }

    /// Deadline scores on the most urgent campaign a channel touches, so a
    /// channel that also happens to rescue tonight's expiry wins.
    #[test]
    fn deadline_uses_the_most_urgent_campaign_a_channel_advances() {
        let campaigns = vec![
            campaign("c-relaxed", "G", Duration::days(20), 60),
            campaign("c-tonight", "G", Duration::hours(3), 60),
            campaign("c-mid", "G", Duration::days(4), 60),
        ];
        let candidates = vec![
            candidate("only-mid", "c-mid", 1),
            multi("relaxed-plus-tonight", &[("c-relaxed", 1), ("c-tonight", 1)]),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Deadline, &w)), "relaxed-plus-tonight");
    }

    /// Priority ranks by the best game a channel serves. Serving a low-ranked
    /// game as well must never drag a top pick down.
    #[test]
    fn priority_ranks_by_the_best_game_a_channel_advances() {
        let campaigns = vec![
            campaign("c-alpha", "Alpha", Duration::days(5), 60),
            campaign("c-omega", "Omega", Duration::days(5), 60),
        ];
        let candidates = vec![
            candidate("omega-only", "c-omega", 1),
            multi("alpha-and-omega", &[("c-alpha", 1), ("c-omega", 1)]),
        ];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string(), "omega".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert_eq!(watched(&choose(&Priority, &w)), "alpha-and-omega");
    }

    #[test]
    fn completionist_uses_the_campaign_nearest_completion() {
        let campaigns = vec![
            campaign("c-mid", "G", Duration::days(5), 300),
            campaign("c-far", "G", Duration::days(5), 900),
            campaign("c-nearly", "G", Duration::days(5), 15),
        ];
        let candidates = vec![
            candidate("mid-only", "c-mid", 1),
            multi("far-and-nearly", &[("c-far", 1), ("c-nearly", 1)]),
        ];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Completionist, &w)), "far-and-nearly");
    }

    /// A candidate whose campaigns we no longer hold cannot be scored, and
    /// must be skipped rather than treated as infinitely urgent.
    #[test]
    fn a_candidate_for_an_unknown_campaign_is_skipped() {
        let campaigns = vec![campaign("c-known", "G", Duration::days(5), 60)];
        let candidates = vec![candidate("ghost", "c-vanished", 1)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert!(matches!(choose(&Deadline, &w), Decision::Idle(_)));
    }

    #[test]
    fn the_choice_explains_itself() {
        let campaigns = vec![
            campaign("c-a", "G", Duration::hours(6), 60),
            campaign("c-b", "G", Duration::days(5), 60),
        ];
        let candidates = vec![multi("broad", &[("c-a", 2), ("c-b", 1)])];
        let favs: HashSet<String> = ["broad".to_string()].into_iter().collect();
        let w = world(&campaigns, &candidates, &favs, &[]);

        let text = explain(chosen(&choose(&Deadline, &w)), &w, &Deadline);
        assert!(text.contains("deadline"), "names the strategy: {text}");
        assert!(text.contains("2 campaigns"), "counts campaigns: {text}");
        assert!(text.contains("favourite"), "mentions favourites: {text}");
    }
}
