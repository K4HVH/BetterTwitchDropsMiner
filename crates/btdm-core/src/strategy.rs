//! Channel selection strategies.
//!
//! A strategy is a pure function over a world snapshot, which is what makes it
//! unit-testable against synthetic scenarios ("47 campaigns, 3 expiring
//! tonight, 2 channels live") and comparable against other strategies without
//! touching Twitch.
//!
//! Every strategy shares the same pipeline: feasibility filter, strategy
//! score, tie-break, hysteresis. Only the score varies.

use crate::eligibility;
use crate::model::*;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;

/// A channel that could be watched, and what it would earn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub channel: Channel,
    pub stream: Stream,
    pub campaign: CampaignId,
    /// Drops this channel would actually progress, per the server.
    pub drops: Vec<DropId>,
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
/// A challenger must beat the incumbent by this much to trigger a switch,
/// so the miner does not thrash between two near-equal channels.
const HYSTERESIS_MARGIN: f64 = 10.0;

/// Runs the shared pipeline and returns what to do.
pub fn choose(strategy: &dyn MiningStrategy, world: &WorldView) -> Decision {
    if world.candidates.is_empty() {
        return Decision::Idle("no live channel can progress any eligible drop".into());
    }

    let mut scored: Vec<(f64, &Candidate)> = Vec::new();
    let mut skipped_infeasible = 0usize;

    for candidate in world.candidates {
        if strategy.respects_feasibility()
            && let Some(campaign) = world.campaign(&candidate.campaign)
            && !is_feasible(campaign, world.now)
        {
            skipped_infeasible += 1;
            continue;
        }

        let Some(base) = strategy.score(candidate, world) else {
            continue;
        };
        scored.push((base + tie_break_bonus(candidate, world), candidate));
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
            .then(b.1.drops.len().cmp(&a.1.drops.len()))
            .then(a.1.channel.login.cmp(&b.1.channel.login))
    });

    let (best_score, best) = scored[0];

    // Hysteresis: stay put unless the challenger is clearly better.
    if let Some(current) = world.current
        && &best.channel.id != current
        && let Some(&(incumbent_score, _)) =
            scored.iter().find(|(_, c)| &c.channel.id == current)
        && best_score - incumbent_score < HYSTERESIS_MARGIN
    {
        return Decision::Idle("staying on the current channel".into());
    }

    Decision::Watch(Box::new(best.clone()))
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
    // Each extra drop advanced simultaneously is free parallelism.
    bonus += candidate.drops.len() as f64;
    // Viewer count as a stability proxy, deliberately tiny.
    bonus += (candidate.stream.viewers as f64).ln_1p() * 0.01;
    bonus
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Earliest deadline first, over campaigns that can still be finished.
/// Nothing achievable is lost to expiry.
pub struct Deadline;

impl MiningStrategy for Deadline {
    fn name(&self) -> &'static str {
        "deadline"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let campaign = world.campaign(&candidate.campaign)?;
        // Less slack scores higher; scaled to hours so the numbers stay sane.
        let hours = slack(campaign, world.now).num_minutes() as f64 / 60.0;
        Some(1000.0 - hours)
    }
}

/// Strict top-down ordering of the user's chosen games.
pub struct Priority;

impl MiningStrategy for Priority {
    fn name(&self) -> &'static str {
        "priority"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let campaign = world.campaign(&candidate.campaign)?;
        let game = campaign.game.name.to_lowercase();
        let rank = world.priority_games.iter().position(|g| *g == game)?;
        // Earlier in the list scores higher.
        Some((world.priority_games.len() - rank) as f64 * 100.0)
    }
}

/// Prefer the channel advancing the most campaigns at once.
pub struct Coverage;

impl MiningStrategy for Coverage {
    fn name(&self) -> &'static str {
        "coverage"
    }

    fn score(&self, candidate: &Candidate, _world: &WorldView) -> Option<f64> {
        Some(candidate.drops.len() as f64 * 100.0)
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
        let campaign = world.campaign(&candidate.campaign)?;
        let remaining = eligibility::remaining_minutes(campaign) as f64;
        // Fewer minutes left scores higher.
        Some(1000.0 - remaining)
    }
}

/// Watch exactly what the user pinned, ignoring everything else.
pub struct Manual;

impl MiningStrategy for Manual {
    fn name(&self) -> &'static str {
        "manual"
    }

    fn score(&self, candidate: &Candidate, world: &WorldView) -> Option<f64> {
        let pinned = world.pinned?.to_lowercase();
        (candidate.channel.login.to_lowercase() == pinned).then_some(1000.0)
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
        "priority" => Some(Box::new(Priority)),
        "coverage" => Some(Box::new(Coverage)),
        "completionist" => Some(Box::new(Completionist)),
        "manual" => Some(Box::new(Manual)),
        _ => None,
    }
}

pub const ALL_STRATEGIES: [&str; 5] = [
    "deadline",
    "priority",
    "coverage",
    "completionist",
    "manual",
];

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

    fn candidate(login: &str, campaign_id: &str, drops: usize) -> Candidate {
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
            campaign: CampaignId::from(campaign_id),
            drops: (0..drops)
                .map(|i| DropId::from(&format!("{campaign_id}-d{i}")[..]))
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
    fn priority_ignores_games_not_on_the_list() {
        let campaigns = vec![campaign("c-x", "Unlisted", Duration::days(5), 60)];
        let candidates = vec![candidate("x", "c-x", 1)];
        let favs = HashSet::new();
        let priority = vec!["alpha".to_string()];
        let w = world(&campaigns, &candidates, &favs, &priority);

        assert!(matches!(choose(&Priority, &w), Decision::Idle(_)));
    }

    #[test]
    fn coverage_prefers_the_channel_advancing_the_most_drops() {
        let campaigns = vec![
            campaign("c-one", "G", Duration::days(5), 60),
            campaign("c-many", "G", Duration::days(5), 60),
        ];
        let candidates = vec![candidate("one", "c-one", 1), candidate("many", "c-many", 4)];
        let favs = HashSet::new();
        let w = world(&campaigns, &candidates, &favs, &[]);

        assert_eq!(watched(&choose(&Coverage, &w)), "many");
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
        }
        assert!(by_name("nonsense").is_none());
    }
}
